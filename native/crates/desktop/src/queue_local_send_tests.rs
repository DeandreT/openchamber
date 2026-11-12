//! Ordinary-send contracts. Runtime fixtures use only test-owned files and hosts.

use super::*;
use crate::queue_storage::{NativeSendIntent, NativeSendKind};

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::draft_content::{
    AgentMention, DraftContext, DraftFile, Mention, SkillRef,
};
use openchamber_native_core::ids::ContextItemId;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::prompt_parts::Metadata;
use openchamber_native_transport::queue_manifest::{
    QueueAgent, QueueLocalFile, QueueLocalItem, QueueMention, QueuePromptContext, QueuePromptParts,
    QueueSkill,
};
use serde_json::json;

use crate::draft_companion::CompanionCapture;

fn configuration() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "selected-model".into(),
        agent: Some("build".into()),
        variant: Some("high".into()),
    }
}

fn owner() -> DraftKey {
    DraftKey::new(
        RuntimeId::from("original-host"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_local")),
    )
    .expect("original session draft")
}

fn client() -> ServerClient {
    ServerClient::new(
        Endpoint::parse("http://localhost:48123/base").expect("captured endpoint"),
        Credential::None,
    )
    .expect("captured authenticated transport")
}

fn target() -> HeldTarget {
    HeldTarget {
        runtime: "original-host".into(),
        directory: "/project".into(),
        temporary_directory: "/tmp".into(),
        server_path: "/tmp/openchamber/0123456789abcdef/original.txt".into(),
        uri: "file:///tmp/openchamber/0123456789abcdef/original.txt".into(),
        filename: "original.txt".into(),
    }
}

fn source() -> SendSourceCapture {
    let mut draft = Draft::new("👩 @build #snippet");
    draft.content.files.push(DraftFile {
        id: AttachmentId::from(format!("{:032x}", 1)),
        name: Some("original.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(8),
        source_document_id: None,
        content: DraftFileContent::Prepared,
    });
    draft.content.agents.push(AgentMention {
        name: "build".into(),
        mention: Some(Mention {
            start: 3,
            end: 9,
            text: "@build".into(),
        }),
    });
    draft.content.skills.push(SkillRef {
        id: "original_skill".into(),
    });
    draft.content.metadata_json = Some(r#"{"native":true}"#.into());
    draft.content.context.push(DraftContext {
        id: ContextItemId::from("native-context"),
        text: "native context".into(),
        description: Some("native description".into()),
        instructions: Some("native instructions".into()),
        metadata_json: Some(r#"{"independent":1}"#.into()),
    });
    let composer = CompanionCapture {
        native: QueueNativeSource {
            owner: owner(),
            draft,
        },
        references: Vec::new(),
    };
    let intent = NativeSendIntent {
        kind: NativeSendKind::Capture,
        directory: "/project".into(),
        id: "msg_local_original".into(),
        delivery: Some(Delivery::Steer),
        companions: Vec::new(),
        native_context_ids: vec!["msg_native_instruction".into(), "msg_native_context".into()],
        files: vec![QueueNativeFileDescriptor {
            id: format!("{:032x}", 1),
            byte_len: 8,
            sha256: sha256(b"original"),
        }],
        edit: None,
    };
    SendSourceCapture {
        version: 1,
        composer,
        intent,
    }
}

fn operation(source: &SendSourceCapture) -> QueueOperation {
    QueueOperation::local_send(
        QueueOperationId::new("ordinary_original").expect("stable local identity"),
        QueueOwner::new(
            &RuntimeId::from("original-host"),
            "/project",
            &SessionId::from("ses_local"),
            &owner(),
        )
        .expect("exact send owner"),
        QueueConfiguration::capture(&configuration()),
        MessageId::from(source.intent.id.clone()),
        source.composer.clone(),
        source.intent.delivery,
        source.intent.native_context_ids.clone(),
        source.intent.files.clone(),
    )
    .expect("typed ordinary send intent")
}

fn inputs(source: &SendSourceCapture) -> PreparedInputs {
    let ids = source
        .intent
        .native_context_ids
        .iter()
        .cloned()
        .map(MessageId::from)
        .collect::<Vec<_>>();
    let mut files = source
        .intent
        .files
        .iter()
        .map(|file| {
            HeldFile::new(
                BlobDescriptor::new(
                    BlobId::parse(&file.id).expect("captured local label"),
                    file.byte_len,
                    file.sha256,
                ),
                &source.composer.native,
            )
        })
        .collect::<Vec<_>>();
    let expanded = QueueLocalItem {
        content: source.composer.native.draft.text.clone(),
        text: "👩 @build expanded-v1".into(),
        files: vec![QueueLocalFile {
            name: "original.txt".into(),
            mime_type: "text/plain".into(),
            byte_len: 8,
            sha256: sha256(b"original"),
            description: None,
            mention: None,
        }],
        send_config: configuration(),
        prompt_parts: QueuePromptParts {
            file_references: Vec::new(),
            agents: vec![QueueAgent {
                name: "build".into(),
                mention: Some(QueueMention {
                    start: 3,
                    end: 9,
                    text: "@build".into(),
                }),
            }],
            skills: vec![QueueSkill {
                id: "original_skill".into(),
                mention: None,
            }],
            metadata: Some(Metadata(
                serde_json::from_value(json!({"native":true})).expect("source metadata"),
            )),
            context: vec![
                QueuePromptContext {
                    id: Some(ids[0].clone()),
                    text: "native instructions".into(),
                    description: None,
                    metadata: None,
                },
                QueuePromptContext {
                    id: Some(ids[1].clone()),
                    text: "native context".into(),
                    description: Some("native description".into()),
                    metadata: Some(Metadata(
                        serde_json::from_value(json!({"independent":1})).expect("context metadata"),
                    )),
                },
            ],
        },
    };
    let expanded = QueueLocalCapture::new("/project", expanded, ids, sha256)
        .expect("once-expanded immutable result");
    files[0].target = Some(target());
    PreparedInputs {
        header: InputHeader {
            version: 1,
            runtime: "original-host".into(),
            endpoint: client().endpoint().as_str().into(),
            directory: "/project".into(),
            session: "ses_local".into(),
            message: source.intent.id.clone(),
            configuration: QueueConfiguration::capture(&configuration()),
            delivery: source.intent.delivery,
            generation: 1,
            revision: 1,
            source_digest: sha256(&serde_json::to_vec(source).expect("full typed source bytes")),
            staged: true,
            sources: vec![InputSource {
                bytes: expanded.bytes().len(),
                digest: expanded.sha256(),
                original: None,
                files,
            }],
        },
        captures: vec![expanded],
    }
}

#[test]
fn typed_input_round_trip_preserves_complete_native_fields_expanded_text_and_targets() {
    let source = source();
    let prepared = inputs(&source);
    let encoded = prepared.encode().expect("immutable binary input record");
    let restored = PreparedInputs::decode(&encoded).expect("exact retained inputs after restart");
    assert_eq!(restored.encode().expect("same typed record"), encoded);
    let capture = &restored.captures[0];
    assert_eq!(capture.item().content, "👩 @build #snippet");
    assert_eq!(capture.item().text, "👩 @build expanded-v1");
    assert_eq!(capture.item().files[0].name, "original.txt");
    assert_eq!(capture.item().files[0].sha256, sha256(b"original"));
    assert_eq!(capture.item().prompt_parts.agents[0].name, "build");
    assert_eq!(capture.item().prompt_parts.skills[0].id, "original_skill");
    assert_eq!(
        capture.item().prompt_parts.context[1]
            .description
            .as_deref(),
        Some("native description")
    );
    assert_eq!(
        capture.context_ids(),
        [
            MessageId::from("msg_native_instruction"),
            MessageId::from("msg_native_context")
        ]
    );
    assert!(restored.header.sources[0].files[0].target.is_some());
    let frozen = restored.freeze(&client()).expect("rich ordinary frame");
    let body: serde_json::Value =
        serde_json::from_slice(frozen.prompt_body()).expect("complete prompt JSON");
    assert_eq!(body["files"][0]["uri"], target().uri);
    assert_eq!(body["text"], "👩 @build expanded-v1");
    assert_eq!(frozen.configuration(), configuration());
    assert_eq!(frozen.context_ids(), capture.context_ids());
}

#[test]
fn native_staging_name_is_retained_without_a_target_or_a_model_facing_path() {
    let source = source();
    let mut prepared = inputs(&source);
    prepared.header.staged = false;
    prepared.header.sources[0].files[0].target = None;
    let mut item = prepared.captures[0].item().clone();
    item.files[0].name = "/server/model-facing/original.txt".into();
    let capture = QueueLocalCapture::new(
        "/project",
        item,
        prepared.captures[0].context_ids().to_vec(),
        sha256,
    )
    .expect("a model-facing name is independent of the local upload label");
    prepared.header.sources[0].bytes = capture.bytes().len();
    prepared.header.sources[0].digest = capture.sha256();
    prepared.captures[0] = capture;
    let restored = PreparedInputs::decode(&prepared.encode().expect("captured safe filename"))
        .expect("original staging name survives restart");
    let file = &restored.header.sources[0].files[0];
    assert_eq!(file.upload_filename(), "original.txt");
    assert_eq!(file.native_filename.as_deref(), Some("original.txt"));
    assert!(file.target.is_none());
    assert!(AttachmentUpload::new(&file.upload_filename(), b"original".to_vec()).is_ok());
    assert!(
        AttachmentUpload::new(
            &restored.captures[0].item().files[0].name,
            b"original".to_vec()
        )
        .is_err()
    );
    let companion_only = HeldFile::new(
        file.descriptor().expect("immutable bytes"),
        &QueueNativeSource {
            owner: owner(),
            draft: Draft::default(),
        },
    );
    assert_eq!(
        companion_only.upload_filename(),
        format!("local-{}.bin", file.id),
        "a companion path never becomes a native upload name"
    );
}

#[test]
fn native_recovery_consumption_restores_the_published_revision_without_an_edit_object() {
    let source = source();
    let mut prepared = inputs(&source);
    prepared.header.generation = 7;
    prepared.header.revision = 19;
    let restored = capture_for_consumption(&source, &prepared.header)
        .expect("verified native-only capture uses its published revision");
    assert_eq!((restored.generation, restored.revision), (7, 19));
    assert!(restored.edit.is_none());
    prepared.header.generation = 0;
    assert!(
        capture_for_consumption(&source, &prepared.header).is_err(),
        "missing generation grants no consumption authority"
    );
}

#[test]
fn restoration_rejects_changed_contexts_delivery_configuration_target_and_capture_metadata() {
    let source = source();
    let mut prepared = inputs(&source);
    let retained = operation(&source);
    let bytes = prepared
        .freeze(&client())
        .expect("original frame")
        .into_bytes();
    assert!(restore_frame(&client(), &retained, &prepared, bytes.clone()).is_ok());
    prepared.header.sources[0].files[0]
        .target
        .as_mut()
        .expect("held target")
        .server_path
        .push_str("-changed");
    assert!(restore_frame(&client(), &retained, &prepared, bytes.clone()).is_err());
    let mut prepared = inputs(&source);
    let mut item = prepared.captures[0].item().clone();
    item.text.push_str(" later snippet version");
    prepared.captures[0] = QueueLocalCapture::new(
        "/project",
        item,
        prepared.captures[0].context_ids().to_vec(),
        sha256,
    )
    .expect("different valid capture");
    assert!(
        restore_frame(&client(), &retained, &prepared, bytes.clone()).is_err(),
        "capture metadata and exact bodies belong to the original inputs"
    );
    let mut other_source = source.clone();
    other_source.intent.delivery = None;
    assert!(
        restore_frame(
            &client(),
            &operation(&other_source),
            &inputs(&source),
            bytes.clone()
        )
        .is_err()
    );
    let mut other_source = source.clone();
    other_source.intent.native_context_ids.swap(0, 1);
    assert!(
        restore_frame(
            &client(),
            &operation(&other_source),
            &inputs(&source),
            bytes.clone()
        )
        .is_err()
    );
    let other = ServerClient::new(
        Endpoint::parse("http://localhost:48124/base").expect("different endpoint"),
        Credential::None,
    )
    .expect("other captured client");
    assert!(restore_frame(&other, &retained, &inputs(&source), bytes).is_err());
}

#[test]
fn request_supersession_needs_all_cancellations_and_no_new_requests_on_the_original_runtime() {
    use openchamber_native_core::request_owner::{RequestOwner, RequestScope};
    let scope = RequestScope::new(RuntimeId::from("original-host"), "/project")
        .expect("captured request location");
    let key = RequestKey {
        scope,
        owner: RequestOwner::Session(SessionId::from("ses_local")),
        id: RequestId::Permission(openchamber_native_core::ids::PermissionId::from(
            "permission_original",
        )),
    };
    assert!(supersession_complete(
        std::slice::from_ref(&key),
        &[],
        true,
        true
    ));
    assert!(!supersession_complete(
        std::slice::from_ref(&key),
        &[],
        true,
        false
    ));
    assert!(!supersession_complete(
        std::slice::from_ref(&key),
        &[],
        false,
        true
    ));
    assert!(
        !supersession_complete(&[], std::slice::from_ref(&key), true, true),
        "a later request blocks dispatch without entering the old cancellation scope"
    );
    assert!(
        supersession_complete(&[], &[], false, true),
        "a request-free task retains its original runtime across selection changes"
    );
}

#[test]
fn check_never_dispatches_and_retry_skips_only_an_observed_leading_context_run() {
    let expected = vec![MessageId::from("msg_first"), MessageId::from("msg_second")];
    assert_eq!(
        observed_step(
            Activation::Check,
            &expected,
            SendObservation::Missing {
                admitted: expected[..1].to_vec()
            }
        )
        .expect("read-only plan"),
        ObservedStep::Missing
    );
    assert_eq!(
        observed_step(
            Activation::Retry,
            &expected,
            SendObservation::Missing {
                admitted: expected[..1].to_vec()
            }
        )
        .expect("explicit retry plan"),
        ObservedStep::Dispatch(expected[..1].to_vec())
    );
    assert!(
        observed_step(
            Activation::Retry,
            &expected,
            SendObservation::Missing {
                admitted: expected[1..].to_vec()
            }
        )
        .is_err()
    );
    assert_eq!(
        observed_step(Activation::Retry, &expected, SendObservation::OutOfOrder)
            .expect("authoritative gap"),
        ObservedStep::OutOfOrder
    );
}

#[test]
fn hydration_preserves_original_runtime_tasks_and_stop_prevents_new_dispatch() {
    let id = QueueOperationId::new("preparing_original").expect("task identity before admission");
    let mut state = LocalSendState::default();
    state.tasks.insert(
        id.clone(),
        LocalSendTask {
            runtime: RuntimeId::from("original-host"),
            session: SessionId::from("ses_local"),
            stopped: false,
            dispatched: false,
            _task: Task::ready(()),
        },
    );
    state.restore(&QueueStorage::default());
    assert!(state.any_busy() && state.busy(&id));
    assert!(state.holds_runtime(&RuntimeId::from("original-host")));
    assert!(!state.holds_runtime(&RuntimeId::from("new-host")));
    assert!(state.stop_before_dispatch(
        &RuntimeId::from("original-host"),
        &SessionId::from("ses_local")
    ));
    assert!(state.tasks[&id].stopped);
    assert!(!state.dispatch_in_flight(
        &RuntimeId::from("original-host"),
        &SessionId::from("ses_local")
    ));
}

#[test]
fn captured_root_release_uses_its_owner_even_when_task_slots_are_absent_or_replaced() {
    let id = QueueOperationId::new("captured_root_owner").expect("exact actor identity");
    let runtime = owner().runtime().clone();
    let session = SessionId::from("ses_local");
    let attachment = AttachmentId::from("named-pre-admission-root");
    let mut state = LocalSendState::default();
    state.captured_files.insert(
        id.clone(),
        CapturedFiles {
            owner: owner(),
            files: vec![attachment.clone()],
        },
    );
    assert!(
        state
            .release_capture(&id, &RuntimeId::from("another-runtime"), &session)
            .is_empty()
    );
    assert!(state.references_native(&attachment));
    assert_eq!(
        state.release_capture(&id, &runtime, &session),
        std::slice::from_ref(&attachment),
        "a missing task slot cannot strand a named capture"
    );
    state.captured_files.insert(
        id.clone(),
        CapturedFiles {
            owner: owner(),
            files: vec![attachment.clone()],
        },
    );
    state.tasks.insert(
        id.clone(),
        LocalSendTask {
            runtime: RuntimeId::from("another-runtime"),
            session: session.clone(),
            stopped: false,
            dispatched: false,
            _task: Task::ready(()),
        },
    );
    state.pending_inputs.insert(id.clone(), vec![7]);
    assert_eq!(
        state.forget_deleted_sessions(
            &runtime,
            std::slice::from_ref(&session),
            std::slice::from_ref(&id)
        ),
        [attachment]
    );
    assert!(
        state.busy(&id),
        "a different-owner task with the same local label stays retained"
    );
    assert_eq!(state.pending_inputs.get(&id), Some(&vec![7]));
}

mod workspace_gates {
    use super::*;

    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use openchamber_native_core::send_config::{
        AgentEntry, CatalogModel, CatalogProvider, ModelChoice,
    };
    use openchamber_native_core::session::{LiveStatus, SessionRecord};
    use openchamber_native_transport::client::ClientDescription;
    #[cfg(unix)]
    use openchamber_native_transport::event::ServerEvent;

    use crate::connection::{Connection, ServerConfig};
    use crate::recovery_tests::Fixture;
    use crate::recovery_view::TransferKind;
    use crate::send_catalog::{DirectoryCatalog, Load};

    /// A real Workspace with test-owned persistence and synthetic session data.
    /// The client is inert: these tests cancel actor tasks in the foreground
    /// before preparation or any network request can run.
    fn workspace(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>, Fixture) {
        workspace_with_fixture(cx, Fixture::new())
    }

    fn workspace_with_fixture(
        cx: &mut TestAppContext,
        fixture: Fixture,
    ) -> (AnyWindowHandle, Entity<Workspace>, Fixture) {
        cx.executor().allow_parking();
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.runtime = owner().runtime().clone();
                workspace.topology.projects[0].path = "/project".into();
                let mut record = SessionRecord::new(SessionId::from("ses_local"), "/project");
                record.title = "Owned local send".into();
                record.model = Some(ModelChoice {
                    provider_id: "provider".into(),
                    model_id: "selected-model".into(),
                    variant: Some("high".into()),
                });
                record.agent = Some("build".into());
                let load = workspace.sessions.begin_load();
                let _ = workspace.sessions.complete_load(load, vec![record]);
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(vec![AgentEntry {
                    description: None,
                    name: "build".into(),
                    display_name: None,
                    mode: None,
                    hidden: false,
                    model: None,
                    color: None,
                }]);
                catalog.providers = Load::Ready(vec![CatalogProvider {
                    id: "provider".into(),
                    name: None,
                    models: vec![CatalogModel {
                        id: "selected-model".into(),
                        provider_model_id: None,
                        name: None,
                        variants: vec!["high".into()],
                        context_limit: None,
                    }],
                }]);
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory("/project".into(), catalog);
                workspace.select_session(SessionId::from("ses_local"), window, cx);
                workspace.open_persistence(fixture.journal(), window, cx);
            });
        })
        .expect("synthetic owner opens");
        crate::recovery_tests::ready(cx, &workspace);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                let client = client();
                let mut connection = Connection::new(ServerConfig {
                    endpoint: client.endpoint().clone(),
                    password: None,
                    client: ClientDescription::default(),
                })
                .expect("test-owned executor");
                connection.client = Some(client);
                connection.state = ConnectionState::Connected;
                connection.loaded = true;
                workspace.connection = Some(connection);
            });
        })
        .expect("inert captured client installs");
        (window, workspace, fixture)
    }

    fn captured_operation() -> QueueOperation {
        QueueOperation::local_send(
            QueueOperationId::new("workspace_local_original").expect("operation identity"),
            QueueOwner::new(
                owner().runtime(),
                "/project",
                &SessionId::from("ses_local"),
                &owner(),
            )
            .expect("exact session owner"),
            QueueConfiguration::capture(&configuration()),
            MessageId::from("msg_workspace_original"),
            CompanionCapture {
                native: QueueNativeSource {
                    owner: owner(),
                    draft: Draft::new("retained rich source"),
                },
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("typed local source")
    }

    /// These phases exercise the projection only. No synthetic frame is granted
    /// persistence, dispatch or byte authority, and teardown discards the fixture.
    fn phase_operation(phase: QueueSendPhase) -> QueueOperation {
        let mut operation =
            serde_json::to_value(captured_operation()).expect("typed operation schema");
        operation["action"]["send"]["phase"] =
            serde_json::to_value(phase).expect("typed phase schema");
        serde_json::from_value(operation).expect("owned projection fixture")
    }

    fn install_operation(workspace: &mut Workspace, operation: QueueOperation) {
        let mut storage =
            serde_json::to_value(QueueStorage::default()).expect("empty ledger schema");
        storage["operations"] = json!([operation]);
        workspace.queue_storage = serde_json::from_value(storage).expect("owned projection ledger");
    }

    fn finish(cx: &mut TestAppContext, window: AnyWindowHandle, workspace: &Entity<Workspace>) {
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                workspace.queue.local_sends = LocalSendState::default();
                workspace.queue_storage = QueueStorage::default();
                workspace.connection = None;
            });
        })
        .expect("projection fixtures release before close");
        crate::recovery_tests::close(cx, window, workspace);
    }

    fn server(workspace: &Workspace) -> QueueServer {
        let connection = workspace
            .connection
            .as_ref()
            .expect("captured inert client");
        QueueServer::new(
            owner().runtime().clone(),
            connection.client.clone().expect("captured endpoint"),
            connection.network.handle(),
            SessionId::from("ses_local"),
            "/project",
        )
        .expect("exact local owner")
    }

    #[cfg(unix)]
    fn native_workspace(
        cx: &mut TestAppContext,
    ) -> (
        AnyWindowHandle,
        Entity<Workspace>,
        Fixture,
        QueueOperationId,
        QueueObjectId,
    ) {
        let fixture = Fixture::new();
        let mut source = source();
        source.composer.native.draft = Draft::new("original native text");
        source.intent.files.clear();
        source.intent.native_context_ids.clear();
        source.intent.delivery = None;
        let (id, frame) = super::durable::seed_native_consumer(&fixture.journal(), &source);
        let (window, workspace, fixture) = workspace_with_fixture(cx, fixture);
        (window, workspace, fixture, id, frame)
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn actual_native_capture_prepares_and_consumes_without_a_document(cx: &mut TestAppContext) {
        let (window, workspace, _fixture, id, frame) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(!workspace.composer_documents.contains(&owner()));
                let capture = workspace
                    .capture_composer_submission(cx)
                    .expect("actual native capture");
                assert!(capture.edit.is_none());
                assert!(capture.generation > 0 && capture.revision > 0);
                assert_eq!(capture.composer.native.draft.text, "original native text");
                assert!(
                    !workspace.composer_documents.contains(&owner()),
                    "native revision authority does not require a spooled Document"
                );
                workspace
                    .change_queue_storage(|queue, workspace| {
                        queue.prepare_local_send(&id, frame, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                    })
                    .expect("the real published source and frame prepare");
                workspace
                    .consume_composer_submission(&id, &capture, window, cx)
                    .expect("an exact native revision is consumable");
                assert!(workspace.composer_draft(cx).text.is_empty());
                assert!(workspace.composer.read(cx).value().is_empty());
                assert!(matches!(
                    workspace
                        .queue_storage
                        .operation(&id)
                        .expect("original consumer remains recoverable")
                        .action(),
                    QueueAction::Send {
                        phase: QueueSendPhase::Prepared { .. },
                        ..
                    }
                ));
            });
        })
        .expect("real native capture and prepared consumption");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn actual_native_prepared_consumption_preserves_typing_undo_and_redo(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt;

        let (window, workspace, _fixture, id, frame) = native_workspace(cx);
        let capture = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let capture = workspace
                        .capture_composer_submission(cx)
                        .expect("actual native submission before typing");
                    workspace.composer.update(cx, |composer, cx| {
                        let end = capture.composer.native.draft.text.len();
                        composer.set_selected_range(end..end, cx);
                        composer.focus(window, cx);
                    });
                    capture
                })
            })
            .expect("native source capture");
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.input(" newer typing", cx);
        })
        .expect("ordinary native typing");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-z"
                } else {
                    "ctrl-z"
                },
                cx,
            )
        })
        .expect("ordinary native Undo");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let current = workspace
                    .capture_composer_submission(cx)
                    .expect("native revision after typing and Undo");
                assert_eq!(
                    current.composer.native.draft.text,
                    capture.composer.native.draft.text
                );
                assert_eq!(current.generation, capture.generation);
                assert!(
                    current.revision > capture.revision,
                    "Undo to identical text is still independent input"
                );
                let checkpoint = workspace.composer.read(cx).history_checkpoint();
                workspace
                    .change_queue_storage(|queue, workspace| {
                        queue.prepare_local_send(&id, frame, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                    })
                    .expect("same published consumer prepares after newer typing");
                workspace
                    .consume_composer_submission(&id, &capture, window, cx)
                    .expect(
                        "newer native authority preserves current input without trapping Capturing",
                    );
                assert_eq!(
                    workspace.composer.read(cx).value().as_ref(),
                    "original native text"
                );
                assert_eq!(
                    workspace.composer.read(cx).history_checkpoint(),
                    checkpoint,
                    "consumption preserves the toolkit's current undo checkpoint"
                );
            });
        })
        .expect("prepared consumption preserves newer native revision");
        cx.update_window(window, |_, window, cx| {
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-shift-z"
                } else {
                    "ctrl-y"
                },
                cx,
            )
        })
        .expect("Redo remains available after older capture consumption");
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                workspace.read(cx).composer.read(cx).value().as_ref(),
                "original native text newer typing"
            )
        });
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn recovered_native_revision_cannot_consume_identical_fresh_input_after_restart(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{EntityInputHandler, Focusable};

        let (window, workspace, _fixture, id, frame) = native_workspace(cx);
        let recovered = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    assert!(!workspace.composer_documents.contains(&owner()));
                    let native = workspace
                        .drafts
                        .get(&owner())
                        .cloned()
                        .expect("the previous process's native draft hydrates");
                    let recovered = ComposerCapture::from_published_at(
                        CompanionCapture {
                            native: QueueNativeSource {
                                owner: owner(),
                                draft: native,
                            },
                            references: Vec::new(),
                        },
                        None,
                        1,
                        3,
                    )
                    .expect("the old published capture restores its previous-process counters");
                    workspace.composer.update(cx, |composer, cx| {
                        composer.focus(window, cx);
                        composer
                            .set_selected_range(0..recovered.composer.native.draft.text.len(), cx);
                    });
                    recovered
                })
            })
            .expect("fresh Workspace selects the restored text without old native counters");
        cx.update_window(window, |_, window, cx| window.press("backspace", cx))
            .expect("ordinary input deletes the restored selection");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |workspace, cx| {
                assert!(
                    workspace
                        .composer
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
                workspace.composer.update(cx, |composer, cx| {
                    composer.replace_text_in_range(None, "original native text", window, cx);
                });
            });
        })
        .expect("independent retyping returns to identical content");
        cx.run_until_parked();
        let checkpoint = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let fresh = workspace
                        .capture_composer_submission(cx)
                        .expect("fresh native authority after delete and retype");
                    assert_eq!(
                        fresh.composer.native.draft.text,
                        recovered.composer.native.draft.text
                    );
                    assert_eq!(
                        (fresh.generation, fresh.revision),
                        (recovered.generation, recovered.revision),
                        "the regression retains the old counters' exact restarted collision"
                    );
                    assert!(
                        workspace
                            .composer
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window)
                    );
                    let checkpoint = workspace.composer.read(cx).history_checkpoint();
                    workspace
                        .change_queue_storage(|queue, workspace| {
                            queue.prepare_local_send(&id, frame, |candidate| {
                                workspace.validate_queue_candidate(candidate)
                            })
                        })
                        .expect("the old exact published consumer prepares");
                    workspace
                        .consume_composer_submission(&id, &recovered, window, cx)
                        .expect("recovery preserves input without trapping the prepared send");
                    assert_eq!(
                        workspace.composer.read(cx).value().as_ref(),
                        "original native text",
                        "restarted local counters cannot authorize clearing independent equal text"
                    );
                    assert_eq!(
                        workspace.composer.read(cx).history_checkpoint(),
                        checkpoint,
                        "the fresh undo history remains intact"
                    );
                    checkpoint
                })
            })
            .expect("recovered native capture preserves the current process's input");
        cx.update_window(window, |_, window, cx| {
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-z"
                } else {
                    "ctrl-z"
                },
                cx,
            )
        })
        .expect("Undo still removes the independent retype");
        cx.run_until_parked();
        cx.read(|cx| {
            let composer = workspace.read(cx).composer.read(cx);
            assert!(composer.value().is_empty());
            assert_ne!(
                composer.history_checkpoint(),
                checkpoint,
                "Undo replays the independent input transaction"
            );
        });
        cx.update_window(window, |_, window, cx| {
            assert!(
                workspace
                    .read(cx)
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "Redo remains routed to the same editor"
            );
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-shift-z"
                } else {
                    "ctrl-y"
                },
                cx,
            );
        })
        .expect("Redo still restores the independent retype");
        cx.run_until_parked();
        cx.read(|cx| {
            let composer = workspace.read(cx).composer.read(cx);
            assert_eq!(composer.value().as_ref(), "original native text");
            assert_eq!(
                composer.history_checkpoint(),
                checkpoint,
                "Redo restores the original independent checkpoint"
            );
        });
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn admitted_local_retirement_retry_needs_no_connection(cx: &mut TestAppContext) {
        let (window, workspace, _fixture, id, frame) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .change_queue_storage(|queue, workspace| {
                        queue.prepare_local_send(&id, frame, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                    })
                    .expect("real local consumer prepares");
                workspace
                    .change_queue_storage(|queue, workspace| {
                        queue.record_send_admitted(
                            &id,
                            &MessageId::from("msg_local_original"),
                            |candidate| workspace.validate_queue_candidate(candidate),
                        )
                    })
                    .expect("exact admitted identity");
                workspace.connection = None;
                workspace.runtime = RuntimeId::from("other-host");
                workspace.retry_local_send_retirement(&id, window, cx);
                assert!(
                    !workspace.queue.local_sends.busy(&id),
                    "another runtime cannot start local retirement"
                );
                workspace.runtime = owner().runtime().clone();
                workspace.retry_local_send(&id, window, cx);
                assert!(
                    workspace.queue.local_sends.busy(&id),
                    "an admitted retry starts local cleanup without HTTP authority"
                );
                // The foreground activation acquires a local task only. Teardown
                // drops it before the first asynchronous cleanup step can run.
                workspace.queue.local_sends = LocalSendState::default();
            });
        })
        .expect("offline captured-owner retirement activation");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    fn kept_local_send(runtime: &RuntimeId, session: &SessionId, suffix: &str) -> QueueOperation {
        let owner = DraftKey::new(
            runtime.clone(),
            "/project",
            DraftTarget::Session(session.clone()),
        )
        .expect("independent session draft");
        QueueOperation::local_send(
            QueueOperationId::new(&format!("delete_kept_{suffix}"))
                .expect("independent operation identity"),
            QueueOwner::new(runtime, "/project", session, &owner).expect("independent exact owner"),
            QueueConfiguration::capture(&configuration()),
            MessageId::from(format!("msg_delete_kept_{suffix}")),
            CompanionCapture {
                native: QueueNativeSource {
                    owner,
                    draft: Draft::new("independent retained source"),
                },
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("typed independent local send")
    }

    #[cfg(unix)]
    fn assert_session_deletion_releases_local_send(
        cx: &mut TestAppContext,
        transition: impl FnOnce(&mut Workspace, &QueueOperationId, QueueObjectId),
    ) {
        let (window, workspace, _fixture, id, frame) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let runtime = owner().runtime().clone();
                let session = SessionId::from("ses_local");
                let other_session = SessionId::from("ses_other");
                workspace.apply_server_events(vec![ServerEvent::SessionCreated(SessionRecord::new(other_session.clone(), "/project"))], window, cx);
                transition(workspace, &id, frame);
                let admitted = matches!(workspace.queue_storage.operation(&id).expect("actual durable consumer").action(),
                    QueueAction::Send { phase: QueueSendPhase::Admitted { .. }, .. });
                let mut kept = Vec::new();
                for (kept_runtime, kept_session, suffix) in [
                    (runtime.clone(), other_session.clone(), "other_session"),
                    (RuntimeId::from("other-host"), session.clone(), "other_runtime"),
                ] {
                    let operation = kept_local_send(&kept_runtime, &kept_session, suffix);
                    let kept_id = operation.id().clone();
                    workspace.change_queue_storage(|queue, workspace| queue.admit_operation(operation,
                        |candidate| workspace.validate_queue_candidate(candidate)).map_err(|(error, _)| error))
                        .expect("independent source passes the full journal preflight");
                    let before = serde_json::to_value(workspace.queue_storage.operation(&kept_id).expect("independent admitted operation"))
                        .expect("independent captured operation");
                    workspace.queue.local_sends.tasks.insert(kept_id.clone(), LocalSendTask { runtime: kept_runtime.clone(),
                        session: kept_session.clone(), stopped: false, dispatched: false, _task: Task::ready(()) });
                    workspace.queue.local_sends.checks.insert(kept_id.clone(), LocalSendCheck::PreparationUnavailable);
                    workspace.queue.local_sends.pending_inputs.insert(kept_id.clone(), suffix.as_bytes().to_vec());
                    kept.push((kept_id, kept_runtime, kept_session, before));
                }
                // Ready task slots exercise exact live-owner scoping without starting
                // preparation, cleanup or HTTP, including on the failing baseline.
                workspace.queue.local_sends.tasks.insert(id.clone(), LocalSendTask { runtime: runtime.clone(),
                    session: session.clone(), stopped: false, dispatched: false, _task: Task::ready(()) });
                // An interrupted RAM encoding grants no byte authority and need
                // not decode successfully for confirmed deletion to cancel it.
                workspace.queue.local_sends.pending_inputs.insert(id.clone(), vec![1, 2, 3]);
                assert!(workspace.prompt_allowed(&workspace.mutation_key(&session)));
                assert!(workspace.payload_local_send_blocks_session(&runtime, &session));
                let captured_server = server(workspace);

                workspace.apply_server_events(vec![ServerEvent::SessionDeleted { session_id: session.clone() }], window, cx);

                assert!(workspace.sessions.get(&session).is_none(), "the authoritative event removes its indexed owner");
                assert!(!workspace.prompt_allowed(&workspace.mutation_key(&session)), "a deleted session cannot authorize prompt retry");
                assert!(workspace.sessions.get(&other_session).is_some(), "an unrelated session stays indexed");
                for (kept_id, kept_runtime, kept_session, before) in kept {
                    assert_eq!(serde_json::to_value(workspace.queue_storage.operation(&kept_id).expect("independent operation survives deletion"))
                        .expect("unchanged independent capture"), before);
                    assert!(workspace.queue.local_sends.busy(&kept_id), "another exact owner's task stays retained");
                    assert_eq!(workspace.queue.local_sends.check(&kept_id), LocalSendCheck::PreparationUnavailable);
                    assert!(workspace.queue.local_sends.pending_inputs.contains_key(&kept_id));
                    assert!(workspace.payload_local_send_blocks_session(&kept_runtime, &kept_session), "independent owners retain their gate");
                }
                if !admitted {
                    assert!(!workspace.queue_storage.operation(&id).is_some_and(|operation|
                        matches!(operation.action(), QueueAction::Send { phase: QueueSendPhase::Admitted { .. }, .. })),
                        "session deletion cannot invent message admission");
                }
                assert!(!workspace.payload_local_send_blocks_session(&runtime, &session),
                    "confirmed deletion releases the local-send owner even when local byte cleanup remains");
                assert!(workspace.queue_storage.local_send_deleted(&id), "the owner is terminal independently of index presence");
                assert!(!workspace.queue.local_sends.busy(&id), "confirmed deletion cancels only its exact live actor");
                assert!(!workspace.queue.local_sends.pending_inputs.contains_key(&id));
                assert!(workspace.transfer_recovery_items().iter().all(|row| row.id != id), "Deleted never projects admission or HTTP recovery actions");
                workspace.runtime = RuntimeId::from("replacement-host");
                assert!(!local_allowed(workspace, &id, &captured_server));
                assert!(!preparing_allowed(workspace, &id, &captured_server));
                assert!(!dispatch_allowed(workspace, &id, &captured_server, &[]), "a late callback cannot regain dispatch after a runtime switch");
                workspace.runtime = runtime;
                assert!(workspace.validate_queue_candidate(&workspace.queue_storage).is_ok(), "deletion leaves a valid full journal candidate");
            });
        }).expect("authoritative session deletion owns local-send cleanup");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn authoritative_session_deletion_releases_capturing_local_send(cx: &mut TestAppContext) {
        assert_session_deletion_releases_local_send(cx, |_, _, _| {});
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn authoritative_session_deletion_releases_prepared_local_send(cx: &mut TestAppContext) {
        assert_session_deletion_releases_local_send(cx, |workspace, id, frame| {
            workspace
                .change_queue_storage(|queue, workspace| {
                    queue.prepare_local_send(id, frame, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })
                .expect("worker-verified source and frame prepare");
        });
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn authoritative_session_deletion_releases_unknown_local_send(cx: &mut TestAppContext) {
        assert_session_deletion_releases_local_send(cx, |workspace, id, frame| {
            workspace
                .change_queue_storage(|queue, workspace| {
                    queue.prepare_local_send(id, frame, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })
                .expect("worker-verified source and frame prepare");
            workspace
                .change_queue_storage(|queue, workspace| {
                    queue.mark_send_unknown(id, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })
                .expect("real dispatch-unknown transition");
        });
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn authoritative_session_deletion_releases_admitted_local_send_task(cx: &mut TestAppContext) {
        assert_session_deletion_releases_local_send(cx, |workspace, id, frame| {
            workspace
                .change_queue_storage(|queue, workspace| {
                    queue.prepare_local_send(id, frame, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })
                .expect("worker-verified source and frame prepare");
            workspace
                .change_queue_storage(|queue, workspace| {
                    queue.record_send_admitted(
                        id,
                        &MessageId::from("msg_local_original"),
                        |candidate| workspace.validate_queue_candidate(candidate),
                    )
                })
                .expect("exact original message admission precedes deletion");
        });
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn authoritative_parent_deletion_releases_descendant_local_send_before_index_removal(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace, _fixture, id, frame) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let session = SessionId::from("ses_local");
                let parent = SessionId::from("ses_deleted_parent");
                let mut child = workspace.sessions.get(&session).expect("retained native owner is indexed").clone();
                child.parent_id = Some(parent.clone());
                workspace.apply_server_events(vec![ServerEvent::SessionCreated(SessionRecord::new(parent.clone(), "/project")),
                    ServerEvent::SessionCreated(child)], window, cx);
                workspace.change_queue_storage(|queue, workspace| queue.prepare_local_send(&id, frame,
                    |candidate| workspace.validate_queue_candidate(candidate))).expect("descendant's worker-verified frame prepares");
                workspace.change_queue_storage(|queue, workspace| queue.mark_send_unknown(&id,
                    |candidate| workspace.validate_queue_candidate(candidate))).expect("descendant retains its unresolved dispatch");
                assert!(workspace.payload_local_send_blocks_session(owner().runtime(), &session));

                workspace.apply_server_events(vec![ServerEvent::SessionDeleted { session_id: parent.clone() }], window, cx);

                assert!(workspace.sessions.get(&parent).is_none());
                assert!(workspace.sessions.get(&session).is_some(), "one parent event exercises cleanup while the descendant record remains available");
                assert!(!workspace.queue_storage.operation(&id).is_some_and(|operation|
                    matches!(operation.action(), QueueAction::Send { phase: QueueSendPhase::Admitted { .. }, .. })),
                    "parent deletion cannot invent admission of the descendant's message");
                assert!(!workspace.payload_local_send_blocks_session(owner().runtime(), &session),
                    "deletion cleanup discovers and releases descendant send owners before applying index removal");
                assert!(workspace.validate_queue_candidate(&workspace.queue_storage).is_ok());
            });
        }).expect("authoritative parent deletion owns descendant cleanup");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn confirmed_delete_completion_after_runtime_switch_uses_only_its_captured_subtree(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_transport::session_actions::Deleted;

        let (window, workspace, _fixture, id, _) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let original = owner().runtime().clone();
                let session = SessionId::from("ses_local");
                let child_id = SessionId::from("ses_captured_child");
                let mut child = SessionRecord::new(child_id.clone(), "/project");
                child.parent_id = Some(session.clone());
                workspace.apply_server_events(vec![ServerEvent::SessionCreated(child)], window, cx);
                let captured = workspace.deleted_session_subtree(&session);
                assert!(captured.contains(&child_id));
                let child = kept_local_send(&original, &child_id, "captured_child");
                let child_operation = child.id().clone();
                workspace.change_queue_storage(|queue, workspace| queue.admit_operation(child,
                    |candidate| workspace.validate_queue_candidate(candidate)).map_err(|(error, _)| error)).expect("captured descendant source");

                let replacement = RuntimeId::from("replacement-host");
                let replacement_child = SessionId::from("ses_replacement_child");
                workspace.switch_runtime(replacement.clone(), window, cx);
                let mut child = SessionRecord::new(replacement_child.clone(), "/project");
                child.parent_id = Some(session.clone());
                let load = workspace.sessions.begin_load();
                let _ = workspace.sessions.complete_load(load, vec![SessionRecord::new(session.clone(), "/project"), child]);
                let mut kept = Vec::new();
                for (target, suffix) in [(&session, "replacement_root"), (&replacement_child, "replacement_child")] {
                    let operation = kept_local_send(&replacement, target, suffix);
                    let kept_id = operation.id().clone();
                    workspace.change_queue_storage(|queue, workspace| queue.admit_operation(operation,
                        |candidate| workspace.validate_queue_candidate(candidate)).map_err(|(error, _)| error)).expect("replacement source");
                    kept.push((kept_id.clone(), serde_json::to_value(workspace.queue_storage.operation(&kept_id)).expect("replacement capture")));
                }
                let current = DraftKey::new(replacement.clone(), "/project", DraftTarget::Session(session.clone())).expect("replacement draft");
                workspace.drafts.save(current.clone(), Draft::new("independent replacement typing"));
                workspace.composer_key = Some(current.clone());
                workspace.restore_composer_draft(window, cx);
                assert_eq!(workspace.composer.read(cx).value().as_ref(), "independent replacement typing",
                    "replacement typing is installed in the actual editor before the captured completion saves it");

                workspace.finish_delete(original.clone(), vec![session.clone()], vec![(session.clone(), captured)],
                    Some(vec![(session.clone(), Ok(Deleted::Removed))]), window, cx);

                assert!(workspace.queue_storage.local_send_deleted(&id));
                assert!(workspace.queue_storage.local_send_deleted(&child_operation));
                assert!(!workspace.payload_local_send_blocks_session(&original, &session));
                assert!(!workspace.payload_local_send_blocks_session(&original, &child_id));
                assert_eq!(workspace.runtime, replacement);
                assert!(workspace.sessions.get(&session).is_some(), "an old completion never deletes the replacement index's same ID");
                assert!(workspace.sessions.get(&replacement_child).is_some(), "replacement descendants grant no old-runtime cleanup authority");
                assert_eq!(workspace.composer_key.as_ref(), Some(&current));
                assert_eq!(workspace.drafts.get(&current).expect("replacement draft remains").text, "independent replacement typing");
                for (kept_id, before) in kept {
                    assert_eq!(serde_json::to_value(workspace.queue_storage.operation(&kept_id)).expect("replacement source remains"), before);
                }
            });
        }).expect("captured-runtime DELETE completion");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn same_runtime_delete_completion_preserves_children_created_after_subtree_capture(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_transport::session_actions::Deleted;

        let (window, workspace, _fixture, root_operation, _) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let runtime = owner().runtime().clone();
                let root = SessionId::from("ses_local");
                let captured_child = SessionId::from("ses_exact_captured_child");
                let late_child = SessionId::from("ses_exact_late_child");
                let other_parent = SessionId::from("ses_exact_other_parent");
                let other_child = SessionId::from("ses_exact_other_child");
                let mut captured_record = SessionRecord::new(captured_child.clone(), "/project");
                captured_record.parent_id = Some(root.clone());
                let mut other_record = SessionRecord::new(other_child.clone(), "/project");
                other_record.parent_id = Some(other_parent.clone());
                workspace.apply_server_events(
                    vec![
                        ServerEvent::SessionCreated(captured_record),
                        ServerEvent::SessionCreated(SessionRecord::new(other_parent, "/project")),
                        ServerEvent::SessionCreated(other_record),
                    ],
                    window,
                    cx,
                );
                let captured = workspace.deleted_session_subtree(&root);
                assert_eq!(captured, [root.clone(), captured_child.clone()]);
                let old_records = workspace
                    .sessions
                    .active()
                    .iter()
                    .chain(workspace.sessions.archived())
                    .cloned()
                    .collect();
                let old_load = workspace.sessions.begin_load();

                // This event arrives while the captured DELETE is in flight.
                // Its new descendant is outside the request's confirmed scope.
                let mut late_record = SessionRecord::new(late_child.clone(), "/project");
                late_record.parent_id = Some(root.clone());
                workspace.apply_server_events(
                    vec![ServerEvent::SessionCreated(late_record)],
                    window,
                    cx,
                );
                let mut retained = Vec::new();
                for (session, suffix, text) in [
                    (
                        &captured_child,
                        "exact_captured_child",
                        "captured child typing",
                    ),
                    (
                        &other_child,
                        "exact_other_child",
                        "independent other child typing",
                    ),
                    (
                        &late_child,
                        "exact_late_child",
                        "independent late child typing",
                    ),
                ] {
                    let operation = kept_local_send(&runtime, session, suffix);
                    let id = operation.id().clone();
                    let draft_owner = operation
                        .owner()
                        .draft_key()
                        .expect("exact child draft owner");
                    workspace
                        .change_queue_storage(|queue, workspace| {
                            queue
                                .admit_operation(operation, |candidate| {
                                    workspace.validate_queue_candidate(candidate)
                                })
                                .map_err(|(error, _)| error)
                        })
                        .expect("independent captured child send");
                    let before = serde_json::to_value(workspace.queue_storage.operation(&id))
                        .expect("retained child send");
                    workspace.drafts.save(draft_owner.clone(), Draft::new(text));
                    workspace.queue.local_sends.tasks.insert(
                        id.clone(),
                        LocalSendTask {
                            runtime: runtime.clone(),
                            session: session.clone(),
                            stopped: false,
                            dispatched: false,
                            _task: Task::ready(()),
                        },
                    );
                    workspace
                        .queue
                        .local_sends
                        .pending_inputs
                        .insert(id.clone(), text.as_bytes().to_vec());
                    retained.push((session.clone(), id, draft_owner, text, before));
                }

                workspace.finish_delete(
                    runtime.clone(),
                    vec![root.clone(), captured_child.clone()],
                    vec![
                        (root.clone(), captured),
                        (captured_child.clone(), vec![captured_child.clone()]),
                    ],
                    Some(vec![
                        (root.clone(), Ok(Deleted::Removed)),
                        (captured_child.clone(), Ok(Deleted::AlreadyGone)),
                    ]),
                    window,
                    cx,
                );

                assert!(workspace.sessions.get(&root).is_none());
                assert!(workspace.sessions.get(&captured_child).is_none());
                assert!(workspace.queue_storage.local_send_deleted(&root_operation));
                for (session, id, draft_owner, text, before) in retained {
                    if session == captured_child {
                        assert!(workspace.queue_storage.local_send_deleted(&id));
                        assert!(workspace.drafts.get(&draft_owner).is_none());
                        assert!(!workspace.queue.local_sends.busy(&id));
                        assert!(!workspace.queue.local_sends.pending_inputs.contains_key(&id));
                    } else {
                        assert!(
                            workspace.sessions.get(&session).is_some(),
                            "a child outside the captured DELETE remains indexed"
                        );
                        assert_eq!(
                            workspace
                                .drafts
                                .get(&draft_owner)
                                .expect("uncaptured child keeps its typing")
                                .text,
                            text
                        );
                        assert_eq!(
                            serde_json::to_value(workspace.queue_storage.operation(&id))
                                .expect("uncaptured child send remains"),
                            before
                        );
                        assert!(
                            workspace.queue.local_sends.busy(&id),
                            "uncaptured child retains its actor"
                        );
                        assert_eq!(
                            workspace
                                .queue
                                .local_sends
                                .pending_inputs
                                .get(&id)
                                .map(Vec::as_slice),
                            Some(text.as_bytes())
                        );
                        assert!(workspace.payload_local_send_blocks_session(&runtime, &session));
                    }
                }
                let update = workspace.sessions.complete_load(old_load, old_records);
                workspace.after_index_update(update, window, cx);
                assert!(
                    workspace.sessions.get(&root).is_none(),
                    "confirmed deletion records the index mutation revision"
                );
                assert!(
                    workspace.sessions.get(&captured_child).is_none(),
                    "a delayed pre-delete list cannot resurrect captured descendants"
                );
                assert!(
                    workspace.sessions.get(&late_child).is_some(),
                    "the newer child survives the delayed list as well as local cleanup"
                );
                assert!(workspace.sessions.get(&other_child).is_some());
            });
        })
        .expect("same-runtime captured-scope DELETE completion");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn failed_or_ambiguous_delete_completion_preserves_local_send_ownership(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_transport::error::TransportError;

        let (window, workspace, _fixture, id, _) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let runtime = owner().runtime().clone();
                let session = SessionId::from("ses_local");
                let before = serde_json::to_value(&workspace.queue_storage)
                    .expect("retained original owner");
                for result in [
                    None,
                    Some(vec![(session.clone(), Err(TransportError::TimedOut))]),
                    Some(vec![(
                        session.clone(),
                        Err(TransportError::Http { status: 500 }),
                    )]),
                ] {
                    workspace.finish_delete(
                        runtime.clone(),
                        vec![session.clone()],
                        vec![(session.clone(), vec![session.clone()])],
                        result,
                        window,
                        cx,
                    );
                    assert_eq!(
                        serde_json::to_value(&workspace.queue_storage)
                            .expect("unconfirmed deletion preserves every source"),
                        before
                    );
                    assert!(!workspace.queue_storage.local_send_deleted(&id));
                    assert!(workspace.payload_local_send_blocks_session(&runtime, &session));
                    assert!(workspace.sessions.get(&session).is_some());
                }
            });
        })
        .expect("failed DELETE never grants local deletion authority");
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    async fn deleted_local_cleanup_needs_no_connection_session_or_readable_source_inputs(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::test::TestAppContextExt;
        use std::time::Duration;

        let (window, workspace, fixture, id, _) = native_workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let source = QueueObjectId::new(&format!("{}-source", id.as_str()))
                    .expect("original source object");
                let object = workspace
                    .queue_storage
                    .object(&source)
                    .expect("worker-verified original source");
                for chunk in object.reference().chunks() {
                    let descriptor = chunk.descriptor().expect("exact retained source chunk");
                    let path = fixture
                        .journal()
                        .parent()
                        .expect("private fixture profile")
                        .join("attachment-spool")
                        .join(format!("blob-{}.bin", descriptor.id().as_str()));
                    std::fs::remove_file(path).expect("test-owned missing source fixture");
                }
                assert!(
                    workspace
                        .queue_storage
                        .object(
                            &QueueObjectId::new(&format!("{}-prepared", id.as_str()))
                                .expect("input identity")
                        )
                        .is_none(),
                    "the interrupted capture never published prepared inputs"
                );
                workspace.apply_server_events(
                    vec![ServerEvent::SessionDeleted {
                        session_id: SessionId::from("ses_local"),
                    }],
                    window,
                    cx,
                );
                workspace.connection = None;
                workspace.runtime = RuntimeId::from("replacement-host");
                workspace.retry_local_send(&id, window, cx);
                assert!(
                    workspace.queue.local_sends.busy(&id),
                    "Deleted retries only its captured local cleanup across runtime selection"
                );
                assert!(!workspace.payload_local_send_blocks_session(
                    owner().runtime(),
                    &SessionId::from("ses_local")
                ));
            });
        })
        .expect("explicit local-only cleanup activation");
        let probe = workspace.clone();
        let retiring = id.clone();
        cx.wait_for(window, Duration::from_secs(10), move |_, cx| {
            let workspace = probe.read(cx);
            !workspace.queue.local_sends.busy(&retiring)
                && workspace.queue_storage.operation(&retiring).is_none()
                && workspace
                    .queue_storage
                    .local_send_retirement(&retiring)
                    .is_none()
        })
        .await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(
                workspace
                    .queue_storage
                    .retired_blobs()
                    .expect("independent named intents")
                    .is_empty()
            );
            assert_eq!(
                workspace.send_attempts.iter().count(),
                0,
                "cleanup creates no legacy or admitted send"
            );
        });
        finish(cx, window, &workspace);
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn deleted_partial_retirement_rolls_back_holds_consumer_catalog_and_all_named_intents(
        cx: &mut TestAppContext,
    ) {
        let fixture = Fixture::new();
        let (id, descriptors) = super::durable::seed_partial_native_consumer(&fixture.journal());
        let (window, workspace, _fixture) = workspace_with_fixture(cx, fixture);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_server_events(
                    vec![ServerEvent::SessionDeleted {
                        session_id: SessionId::from("ses_local"),
                    }],
                    window,
                    cx,
                );
                let files = workspace
                    .queue_storage
                    .composer_file_descriptors(&id)
                    .expect("all exact partial cleanup obligations");
                workspace
                    .change_queue_storage(|queue, workspace| {
                        queue.begin_local_send_retirement(&id, files, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                    })
                    .expect("deleted cleanup continuation");
                let resources = descriptors
                    .iter()
                    .map(|descriptor| (AttachmentId::from(descriptor.id().as_str()), *descriptor))
                    .collect::<HashMap<_, _>>();
                let old_retired = workspace.retired_attachment_blobs.clone();
                workspace.retired_attachment_blobs = (0..crate::mutation_journal::MAX_ENTRIES)
                    .map(|index| {
                        BlobId::parse(&format!(
                            "{:032x}",
                            0xf000_0000_0000_0000u128 + index as u128
                        ))
                        .expect("distinct capacity fixture label")
                    })
                    .collect();
                workspace
                    .validate_queue_candidate(&workspace.queue_storage)
                    .expect("full retirement capacity remains a valid retained state");
                let queue_before = serde_json::to_value(&workspace.queue_storage)
                    .expect("terminal consumer and continuation");
                let catalog_before = workspace.attachments.clone();
                let holds_before = workspace.attachment_preparations.clone();
                let retired_before = workspace.retired_attachment_blobs.clone();
                assert!(
                    prepare_terminal_retirement(workspace, std::slice::from_ref(&id), &resources)
                        .is_err()
                );
                assert_eq!(
                    serde_json::to_value(&workspace.queue_storage).expect("consumer rollback"),
                    queue_before
                );
                assert!(
                    workspace.attachments == catalog_before,
                    "capacity failure restores catalog custody"
                );
                assert_eq!(
                    workspace.attachment_preparations, holds_before,
                    "capacity failure restores the exact unpublished hold"
                );
                assert_eq!(
                    workspace.retired_attachment_blobs, retired_before,
                    "no partial retirement intents escape refusal"
                );

                workspace.retired_attachment_blobs = old_retired;
                let plan =
                    prepare_terminal_retirement(workspace, std::slice::from_ref(&id), &resources)
                        .expect("explicit retry plans one atomic retirement");
                assert!(workspace.queue_storage.operation(&id).is_none());
                assert!(workspace.attachment_preparations.is_empty());
                assert!(workspace.attachments.is_empty());
                assert_eq!(
                    plan.attachments.len(),
                    2,
                    "cataloged and Proposed bytes both receive exact durable deletion intents"
                );
                for descriptor in &descriptors {
                    assert!(workspace.retired_attachment_blobs.contains(descriptor.id()));
                }
                assert!(
                    workspace
                        .queue_storage
                        .local_send_retirement(&id)
                        .expect("continuation survives consumer removal")
                        .deleted
                );
                workspace
                    .validate_queue_candidate(&workspace.queue_storage)
                    .expect("consumer, holds and newly unowned intents change together");
            });
        })
        .expect("partial deletion retirement rollback and local retry");
        finish(cx, window, &workspace);
    }

    #[gpui_kit::test]
    fn dispatch_gate_allows_its_own_send_and_rejects_changed_native_upload_names(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace, _fixture) = workspace(cx);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                let operation = captured_operation();
                let id = operation.id().clone();
                install_operation(workspace, operation);
                workspace.queue.local_sends.tasks.insert(
                    id.clone(),
                    LocalSendTask {
                        runtime: owner().runtime().clone(),
                        session: SessionId::from("ses_local"),
                        stopped: false,
                        dispatched: false,
                        _task: Task::ready(()),
                    },
                );
                let server = server(workspace);
                assert!(
                    workspace.queue_preview_pending(&SessionId::from("ses_local")),
                    "the broad preview gate includes this operation"
                );
                assert!(
                    dispatch_allowed(workspace, &id, &server, &[]),
                    "an ordinary actor cannot block itself through queue preview state"
                );
                workspace
                    .queue
                    .local_sends
                    .stop_before_dispatch(owner().runtime(), &SessionId::from("ses_local"));
                assert!(!dispatch_allowed(workspace, &id, &server, &[]));

                let source = source();
                let retained = super::operation(&source);
                let mut prepared = inputs(&source);
                assert!(prepared.validate(&retained, &source, &server).is_ok());
                prepared.header.sources[0].files[0].native_filename = Some("changed.txt".into());
                assert!(
                    prepared.validate(&retained, &source, &server).is_err(),
                    "an input object cannot replace the original native filename"
                );
                let mut prepared = inputs(&source);
                prepared.header.sources[0].files[0].target = Some(HeldTarget {
                    filename: "changed.txt".into(),
                    ..target()
                });
                assert!(
                    prepared.validate(&retained, &source, &server).is_err(),
                    "a staged native target must keep its captured draft name"
                );
            });
        })
        .expect("actor dispatch and exact staging-name boundaries");
        finish(cx, window, &workspace);
    }

    #[gpui_kit::test]
    fn stop_during_one_supersession_keeps_unattempted_requests_pending(cx: &mut TestAppContext) {
        use openchamber_native_core::ids::PermissionId;
        use openchamber_native_core::pending_requests::{
            PendingRequest, PendingRequests, ResponseState,
        };
        use openchamber_native_core::permission::PermissionRequest;
        use openchamber_native_core::request_owner::RequestScope;

        let (window, workspace, _fixture) = workspace(cx);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                let operation = captured_operation();
                let id = operation.id().clone();
                install_operation(workspace, operation);
                workspace.queue.local_sends.tasks.insert(
                    id.clone(),
                    LocalSendTask {
                        runtime: owner().runtime().clone(),
                        session: SessionId::from("ses_local"),
                        stopped: false,
                        dispatched: false,
                        _task: Task::ready(()),
                    },
                );
                let server = server(workspace);
                let scope = RequestScope::new(owner().runtime().clone(), "/project")
                    .expect("captured request scope");
                let mut store = PendingRequests::new(scope.clone());
                let mut captured = Vec::new();
                for suffix in ["first", "middle", "last"] {
                    let request = PendingRequest::Permission(PermissionRequest {
                        id: PermissionId::from(format!("permission_{suffix}")),
                        session_id: SessionId::from("ses_local"),
                        action: "read".into(),
                        resources: Vec::new(),
                        save: Vec::new(),
                        source: None,
                        message: None,
                        metadata_display: None,
                    });
                    captured.push(request.key(&scope));
                    assert!(store.asked(request));
                }
                workspace.blocking.stores.insert(scope.clone(), store);
                let attempted = prepare_next_supersession(workspace, &id, &server, &captured[0])
                    .expect("first explicit cancellation locks one request")
                    .expect("the first request is still pending");
                let store = &workspace.blocking.stores[&scope];
                assert_eq!(
                    store.get(&captured[0]).expect("attempted request").response,
                    ResponseState::Responding
                );
                for request in &captured[1..] {
                    assert_eq!(
                        store.get(request).expect("unattempted request").response,
                        ResponseState::Pending
                    );
                }
                assert!(
                    workspace
                        .queue
                        .local_sends
                        .stop_before_dispatch(owner().runtime(), &SessionId::from("ses_local"))
                );
                workspace
                    .blocking
                    .stores
                    .get_mut(&scope)
                    .expect("same request owner")
                    .reply_failed(attempted, ReplyFailure::Unknown)
                    .expect("an attempted unknown result keeps exact-read recovery");
                for request in &captured[1..] {
                    assert!(
                        prepare_next_supersession(workspace, &id, &server, request).is_err(),
                        "Stop prevents the next lock and HTTP attempt"
                    );
                    assert_eq!(
                        workspace.blocking.stores[&scope]
                            .get(request)
                            .expect("unattempted request remains actionable")
                            .response,
                        ResponseState::Pending
                    );
                }
                let store = &workspace.blocking.stores[&scope];
                assert_eq!(
                    store
                        .get(&captured[0])
                        .expect("attempted request remains uncertain")
                        .response,
                    ResponseState::Uncertain
                );
            });
        })
        .expect("per-request cancellation ownership");
        finish(cx, window, &workspace);
    }

    #[gpui_kit::test]
    fn real_workspace_blocks_same_owner_sends_without_self_blocking_enqueue_gate(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace, _fixture) = workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let runtime = owner().runtime().clone();
                let session = SessionId::from("ses_local");
                install_operation(workspace, captured_operation());
                assert!(workspace.payload_local_send_blocks_session(&runtime, &session));
                assert!(workspace.queue_preview_pending(&session));
                assert!(
                    !workspace.payload_enqueue_blocks_session(&runtime, &session),
                    "the actor's enqueue-only gate excludes its own Send"
                );
                assert!(
                    !workspace.payload_local_send_blocks_session(
                        &RuntimeId::from("other-host"),
                        &session
                    )
                );
                assert!(
                    !workspace
                        .payload_local_send_blocks_session(&runtime, &SessionId::from("ses_other"))
                );
                assert!(!workspace.can_send_connected());
                let connection = workspace
                    .connection
                    .as_ref()
                    .expect("captured test transport");
                let transport = (
                    connection.client.clone().expect("inert client"),
                    connection.network.handle(),
                );
                assert!(
                    !workspace.send_outgoing(
                        workspace.mutation_key(&session),
                        owner(),
                        crate::session_mutations::Outgoing {
                            draft: Draft::new("legacy background capture"),
                            text: "legacy background capture".into(),
                            parts: Default::default(),
                            model: None,
                            agent: None,
                            delivery: None,
                            consumes_composer: false,
                            supersedes: false,
                            reserved_id: None,
                            user_effort: None,
                        },
                        transport,
                        window,
                        cx
                    ),
                    "a legacy outgoing identity cannot overlap the local actor"
                );
                workspace.composer.update(cx, |composer, cx| {
                    composer.set_value("newer independent typing", window, cx)
                });
                workspace.submit_connected_now(window, cx);
                workspace.submit_from_enter(false, window, cx);
                workspace.queue_composer(window, cx);
                workspace.edit_queued(
                    &openchamber_native_core::message_queue::QueuedItemId::new("unrelated-row"),
                    window,
                    cx,
                );
                workspace.send_now(None, window, cx);
                assert!(
                    !workspace.queue.any_payload_busy(),
                    "no action acquires another actor while the local source is unresolved"
                );
                assert_eq!(
                    workspace.send_attempts.iter().count(),
                    0,
                    "blocked ordinary sends never fall back to legacy Outgoing"
                );
                assert_eq!(
                    workspace.composer.read(cx).value().as_ref(),
                    "newer independent typing"
                );

                workspace.queue_storage = QueueStorage::default();
                let id =
                    QueueOperationId::new("workspace_preparing").expect("pre-ledger task identity");
                workspace.queue.local_sends.tasks.insert(
                    id,
                    LocalSendTask {
                        runtime: runtime.clone(),
                        session: session.clone(),
                        stopped: false,
                        dispatched: false,
                        _task: Task::ready(()),
                    },
                );
                assert!(
                    workspace.payload_local_send_blocks_session(&runtime, &session),
                    "pre-publication capture already holds its session"
                );
                assert!(workspace.queue_preview_pending(&session));
            });
        })
        .expect("real Workspace gates");
        finish(cx, window, &workspace);
    }

    #[gpui_kit::test]
    fn real_selected_submit_uses_local_actor_for_context_only_content_and_keeps_busy_policy(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace, _fixture) = workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let session = SessionId::from("ses_local");
                workspace.submit_connected_now(window, cx);
                assert!(
                    !workspace.queue.local_sends.any_busy(),
                    "empty composer cannot acquire an ordinary send"
                );
                let mut draft = Draft::default();
                draft.content.context.push(DraftContext {
                    id: ContextItemId::from("context-only-original"),
                    text: "retained context".into(),
                    description: Some("selection".into()),
                    instructions: Some("original instruction".into()),
                    metadata_json: Some(r#"{"source":"selection"}"#.into()),
                });
                workspace.drafts.save(owner(), draft.clone());
                assert!(workspace.composer_has_sendable_content(cx));
                workspace
                    .live_status
                    .insert(session.clone(), LiveStatus::Busy);
                workspace.submit_connected(window, cx);
                assert!(
                    !workspace.queue.local_sends.any_busy(),
                    "plain busy submit without requests retains existing policy"
                );
                workspace.submit_connected_now(window, cx);
                assert!(
                    workspace.queue.local_sends.any_busy(),
                    "send-now starts the rich actor for context-only input"
                );
                assert_eq!(
                    workspace.send_attempts.iter().count(),
                    0,
                    "the ordinary selected path creates no legacy attempt"
                );
                assert_eq!(
                    workspace.composer_draft(cx),
                    draft,
                    "actor preparation retains the entire original draft"
                );
                // Forget the first activation entirely: its task and its journaled
                // Send would otherwise hold the session, as they should.
                workspace.queue.local_sends = LocalSendState::default();
                workspace.queue_storage = QueueStorage::default();
                workspace.composer_documents.clear_submissions_for_test();
                workspace.live_status.insert(session, LiveStatus::Idle);
                workspace.submit_connected_with(Some(Delivery::Steer), window, cx);
                assert!(
                    !workspace.queue.local_sends.any_busy(),
                    "steer is admitted only while busy"
                );
                workspace.submit_connected(window, cx);
                assert!(
                    workspace.queue.local_sends.any_busy(),
                    "idle ordinary submission uses the same rich actor"
                );
                workspace.queue.local_sends = LocalSendState::default();

                let mut unsupported = Draft::new("keep the complete source after actor refusal");
                unsupported.content.files.push(DraftFile {
                    id: AttachmentId::from("unprepared-reference"),
                    name: Some("original.txt".into()),
                    media_type: Some("text/plain".into()),
                    byte_len: None,
                    source_document_id: None,
                    content: DraftFileContent::Reference {
                        uri: "file:///project/original.txt".into(),
                    },
                });
                workspace.drafts.save(owner(), unsupported.clone());
                workspace.restore_composer_draft(window, cx);
                workspace.submit_connected(window, cx);
                assert!(
                    !workspace.queue.local_sends.any_busy(),
                    "unprepared file refuses before actor admission"
                );
                assert_eq!(
                    workspace.send_attempts.iter().count(),
                    0,
                    "actor refusal has no narrow Outgoing fallback"
                );
                assert_eq!(workspace.composer_draft(cx), unsupported);
            });
        })
        .expect("ordinary selected submission entrypoint");
        finish(cx, window, &workspace);
    }

    #[gpui_kit::test]
    fn real_workspace_recovery_projects_every_local_send_status_and_explicit_preparation_retry(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace, _fixture) = workspace(cx);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                let frame =
                    QueueObjectId::new("workspace_original-send").expect("projection frame label");
                let id = captured_operation().id().clone();
                for (check, expected, retry) in [
                    (
                        LocalSendCheck::Unknown,
                        "chat.nativeRecovery.transferUnknown",
                        true,
                    ),
                    (
                        LocalSendCheck::Working,
                        "chat.nativeRecovery.transferWorking",
                        true,
                    ),
                    (
                        LocalSendCheck::Missing,
                        "chat.nativeRecovery.notObserved",
                        true,
                    ),
                    (LocalSendCheck::Admitted, "chat.nativeRecovery.queued", true),
                    (
                        LocalSendCheck::OutOfOrder,
                        "chat.nativeRecovery.outOfOrder",
                        false,
                    ),
                    (
                        LocalSendCheck::PreparationUnavailable,
                        "chat.nativeRecovery.sendFailed",
                        true,
                    ),
                    (
                        LocalSendCheck::Failed,
                        "chat.nativeRecovery.transferFailed",
                        true,
                    ),
                ] {
                    install_operation(
                        workspace,
                        phase_operation(QueueSendPhase::Unknown {
                            frame: frame.clone(),
                        }),
                    );
                    workspace.queue.local_sends.checks.insert(id.clone(), check);
                    let rows = workspace.transfer_recovery_items();
                    assert_eq!(
                        rows.len(),
                        1,
                        "each owned operation projects one recovery row"
                    );
                    let row = &rows[0];
                    assert_eq!(row.id, id);
                    assert_eq!(row.kind, TransferKind::LocalSend);
                    assert_eq!(row.session_label.as_ref(), "Owned local send");
                    assert_eq!(row.status, expected);
                    assert!(
                        row.can_check,
                        "a nonbusy frozen capture permits read-only Check"
                    );
                    assert_eq!(
                        row.can_retry, retry,
                        "OutOfOrder alone removes retry authority"
                    );
                }
                install_operation(workspace, captured_operation());
                workspace.queue.local_sends.checks.clear();
                let rows = workspace.transfer_recovery_items();
                assert_eq!(rows[0].status, "chat.nativeRecovery.preparing");
                assert!(
                    !rows[0].can_check,
                    "Capturing has no frozen frame to observe"
                );
                assert!(
                    rows[0].can_retry,
                    "only explicit Retry resumes local preparation"
                );
                workspace.queue.local_sends.tasks.insert(
                    id.clone(),
                    LocalSendTask {
                        runtime: owner().runtime().clone(),
                        session: SessionId::from("ses_local"),
                        stopped: false,
                        dispatched: false,
                        _task: Task::ready(()),
                    },
                );
                let rows = workspace.transfer_recovery_items();
                assert!(
                    !rows[0].can_check && !rows[0].can_retry,
                    "a live actor disables both recovery actions"
                );
                workspace.queue.local_sends.tasks.clear();
                install_operation(
                    workspace,
                    phase_operation(QueueSendPhase::Admitted { frame }),
                );
                let rows = workspace.transfer_recovery_items();
                assert_eq!(
                    rows[0].status, "chat.nativeRecovery.queued",
                    "hydrated terminal admission outranks absent check metadata"
                );
                assert!(!workspace.payload_local_send_blocks_session(
                    owner().runtime(),
                    &SessionId::from("ses_local")
                ));
                workspace.runtime = RuntimeId::from("other-host");
                assert!(
                    workspace.transfer_recovery_items().is_empty(),
                    "another runtime never exposes these recovery actions"
                );
                workspace.runtime = owner().runtime().clone();
                workspace
                    .connection
                    .as_mut()
                    .expect("inert connection")
                    .state = ConnectionState::Reconnecting;
                let rows = workspace.transfer_recovery_items();
                assert!(
                    !rows[0].can_check && !rows[0].can_retry,
                    "disconnection preserves the row without action authority"
                );
            });
        })
        .expect("real Workspace recovery projection");
        finish(cx, window, &workspace);
    }

    #[gpui_kit::test]
    fn real_stop_cancels_pre_dispatch_but_waits_for_dispatched_and_unknown_local_sends(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace, _fixture) = workspace(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let runtime = owner().runtime().clone();
                let session = SessionId::from("ses_local");
                let key = workspace.mutation_key(&session);
                let id = captured_operation().id().clone();
                workspace.queue.local_sends.tasks.insert(
                    id.clone(),
                    LocalSendTask {
                        runtime: runtime.clone(),
                        session: session.clone(),
                        stopped: false,
                        dispatched: true,
                        _task: Task::ready(()),
                    },
                );
                workspace.abort_connected(window, cx);
                assert!(
                    workspace.abort_tasks.is_empty(),
                    "Stop never overlaps a dispatched local HTTP chain"
                );
                assert!(
                    !workspace.queue.local_sends.tasks[&id].stopped,
                    "a dispatched chain finishes before interruption"
                );
                assert!(
                    workspace.send_attempts.can_send(&key),
                    "a refused interrupt acquires no legacy abort slot"
                );
                workspace.queue.local_sends.tasks.clear();
                install_operation(workspace, captured_operation());
                workspace.abort_connected(window, cx);
                assert!(
                    workspace.abort_tasks.is_empty(),
                    "an idle unresolved local operation blocks abort conservatively"
                );
                workspace.queue_storage = QueueStorage::default();
                workspace.queue.local_sends.tasks.insert(
                    id.clone(),
                    LocalSendTask {
                        runtime,
                        session,
                        stopped: false,
                        dispatched: false,
                        _task: Task::ready(()),
                    },
                );
                // No HTTP client is needed to establish that pre-dispatch Stop
                // cancels the actor before considering an interrupt request.
                workspace.connection = None;
                workspace.abort_connected(window, cx);
                assert!(workspace.queue.local_sends.tasks[&id].stopped);
                assert!(workspace.abort_tasks.is_empty());
            });
        })
        .expect("real Workspace Stop boundary");
        finish(cx, window, &workspace);
    }
}

#[cfg(unix)]
mod durable {
    use super::*;

    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::attachment_catalog::{PreparationHold, PreparedAttachment};
    use crate::attachment_io::{AttachmentFailure, AttachmentJournal};
    use crate::attachment_spool::SpoolError;
    use crate::journal_worker::{BlobFailure, CommitOutcome, JournalWorker};
    use crate::mutation_journal::{JournalCommit, JournalSnapshot, LoadedJournal};
    use crate::queue_capture::QueueCaptureBytes;
    use crate::queue_storage::QueueObject;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Profile(PathBuf);
    impl Profile {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/tmp/opencode/ordinary-send-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private test-owned profile");
            Self(path)
        }
        fn journal(&self) -> PathBuf {
            self.0.join("journal.json")
        }
        fn blob(&self, id: &BlobId) -> PathBuf {
            self.0
                .join("attachment-spool")
                .join(format!("blob-{}.bin", id.as_str()))
        }
    }
    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[track_caller]
    fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
        let result = journal.save(snapshot);
        assert!(
            matches!(&result, Ok(JournalCommit::Durable)),
            "canonical full snapshot commits: {result:?}"
        );
    }

    fn plan(
        snapshot: &mut JournalSnapshot,
        id: &QueueOperationId,
        role: &str,
        bytes: Vec<u8>,
        kind: QueueObjectKind,
        label: u128,
    ) -> (QueueObjectId, QueueCaptureBytes) {
        let blob = BlobId::parse(&format!("{label:032x}")).expect("local-only chunk label");
        let capture = QueueCaptureBytes::new(kind, bytes, &[blob]).expect("small exact capture");
        let object =
            QueueObjectId::new(&format!("{}-{role}", id.as_str())).expect("original role identity");
        assert!(
            snapshot
                .queue
                .admit_object(
                    QueueObject::planned(
                        object.clone(),
                        id.clone(),
                        kind,
                        1,
                        capture.reference().clone()
                    )
                    .expect("retained original chunk labels"),
                    |_| Ok(())
                )
                .is_ok()
        );
        (object, capture)
    }

    fn publish(
        journal: &mut AttachmentJournal,
        snapshot: &mut JournalSnapshot,
        object: &QueueObjectId,
        capture: QueueCaptureBytes,
        source: bool,
    ) {
        save(journal, snapshot);
        let hold = snapshot
            .queue
            .object(object)
            .expect("owned capture")
            .hold(0)
            .expect("unpublished chunk");
        let receipt = journal
            .put_queue(&hold, capture.bytes())
            .expect("actual private spool write");
        snapshot
            .queue
            .record_chunk(&hold, &receipt)
            .expect("exact durable chunk");
        save(journal, snapshot);
        let proof = journal
            .verify_queue(
                &snapshot
                    .queue
                    .object(object)
                    .expect("same capture")
                    .verification(),
            )
            .expect("whole-stream proof");
        if source {
            snapshot
                .queue
                .publish_send_source(&hold.operation, &proof, |_| Ok(()))
                .expect("typed compound publication");
        } else {
            snapshot
                .queue
                .record_verified(&proof)
                .expect("verified private capture");
        }
        save(journal, snapshot);
    }

    fn fixture(
        journal: &mut AttachmentJournal,
    ) -> (JournalSnapshot, QueueOperationId, PreparedInputs) {
        let source = source();
        let inputs = inputs(&source);
        let id = operation(&source).id().clone();
        let mut snapshot = JournalSnapshot::default();
        let descriptor = inputs.header.sources[0].files[0]
            .descriptor()
            .expect("actual held file");
        let hold = PreparationHold {
            owner: owner(),
            id: AttachmentId::from(descriptor.id().as_str()),
            generation: 1,
            blob: *descriptor.id(),
        };
        snapshot.preparations.push(hold.clone());
        save(journal, &snapshot);
        let receipt = journal
            .put(&hold, b"original")
            .expect("actual prepared bytes");
        assert!(matches!(
            receipt.commit,
            crate::attachment_spool::SpoolCommit::Durable
        ));
        assert_eq!(
            receipt.descriptor, descriptor,
            "the real worker receipt supplies exact prepared-file custody"
        );
        snapshot.preparations.clear();
        snapshot.attachments.insert(
            hold.id,
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: Some(target().restore(StagingPhase::Ready)),
            },
        );
        // Publish the unchanged Prepared draft only after its bytes have catalog
        // custody. The earlier durable hold authorizes preparation alone.
        snapshot
            .drafts
            .push((owner(), source.composer.native.draft.clone()));
        assert!(
            snapshot
                .queue
                .admit_operation(operation(&source), |_| Ok(()))
                .is_ok()
        );
        let (object, capture) = plan(
            &mut snapshot,
            &id,
            "source",
            serde_json::to_vec(&source).expect("complete SendSourceCapture"),
            QueueObjectKind::NativeCompanion,
            101,
        );
        publish(journal, &mut snapshot, &object, capture, true);
        for (role, label) in [("inputs", 102), ("prepared", 103)] {
            let (object, capture) = plan(
                &mut snapshot,
                &id,
                role,
                inputs.encode().expect("expanded immutable inputs"),
                QueueObjectKind::NativeCompanion,
                label,
            );
            publish(journal, &mut snapshot, &object, capture, false);
        }
        (snapshot, id, inputs)
    }

    pub(super) fn seed_native_consumer(
        path: &std::path::Path,
        source: &SendSourceCapture,
    ) -> (QueueOperationId, QueueObjectId) {
        let (mut journal, _) =
            AttachmentJournal::open(path).expect("private native-consumption journal");
        let mut snapshot = JournalSnapshot::default();
        snapshot
            .drafts
            .push((owner(), source.composer.native.draft.clone()));
        let retained = operation(source);
        let id = retained.id().clone();
        assert!(snapshot.queue.admit_operation(retained, |_| Ok(())).is_ok());
        let (object, capture) = plan(
            &mut snapshot,
            &id,
            "source",
            serde_json::to_vec(source).expect("typed original native capture"),
            QueueObjectKind::NativeCompanion,
            101,
        );
        publish(&mut journal, &mut snapshot, &object, capture, true);
        let local = QueueLocalCapture::new(
            "/project",
            QueueLocalItem {
                content: source.composer.native.draft.text.clone(),
                text: source.composer.native.draft.text.clone(),
                files: Vec::new(),
                send_config: configuration(),
                prompt_parts: QueuePromptParts::default(),
            },
            Vec::new(),
            sha256,
        )
        .expect("complete native-only payload");
        let sources = [QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &local,
            files: &[],
        })];
        let frozen = QueueFrozenSend::capture_local(
            &client(),
            QueueLocalSendInput {
                session: &SessionId::from("ses_local"),
                directory: "/project",
                message_id: &MessageId::from(source.intent.id.clone()),
                sources: &sources,
                selected_input: 0,
                delivery: source.intent.delivery,
            },
            sha256,
        )
        .expect("real native-only frozen frame");
        let (frame, capture) = plan(
            &mut snapshot,
            &id,
            "send",
            frozen.into_bytes(),
            QueueObjectKind::FrozenSend,
            102,
        );
        publish(&mut journal, &mut snapshot, &frame, capture, false);
        (id, frame)
    }

    pub(super) fn seed_partial_native_consumer(
        path: &std::path::Path,
    ) -> (QueueOperationId, Vec<BlobDescriptor>) {
        use crate::queue_storage::{
            QueueComposerPreparation, QueueDerivedFile, QueuePreparationMention,
            QueuePreparationSource, QueuePreparationSourceId,
        };

        let (mut journal, _) =
            AttachmentJournal::open(path).expect("private partial native journal");
        let mut source = source();
        source.composer.native.draft = Draft::new("@first.txt @later.txt");
        source.intent.files.clear();
        source.intent.native_context_ids.clear();
        source.intent.delivery = None;
        let input = QueueLocalCapture::new(
            "/project",
            QueueLocalItem {
                content: source.composer.native.draft.text.clone(),
                text: source.composer.native.draft.text.clone(),
                files: Vec::new(),
                send_config: configuration(),
                prompt_parts: QueuePromptParts::default(),
            },
            Vec::new(),
            sha256,
        )
        .expect("captured explicit input mentions");
        let mut operation = operation(&source);
        operation
            .begin_composer_preparation(1, 1)
            .expect("actual initial preparation marker");
        let id = operation.id().clone();
        let seal = *operation
            .source_sha256()
            .expect("immutable original source seal");
        let mut snapshot = JournalSnapshot::default();
        snapshot
            .drafts
            .push((owner(), source.composer.native.draft.clone()));
        assert!(
            snapshot
                .queue
                .admit_operation(operation, |_| Ok(()))
                .is_ok()
        );
        let files = [
            (0, 10, "@first.txt", "first.txt", 720u128),
            (11, 21, "@later.txt", "later.txt", 721u128),
        ]
        .into_iter()
        .map(|(start, end, text, name, label)| QueueDerivedFile {
            source: QueuePreparationSourceId::Source(0),
            mention: QueuePreparationMention {
                start,
                end,
                text: text.into(),
            },
            server_path: format!("/project/{name}"),
            raw_source_byte_len: 7,
            raw_source_sha256: sha256(b"derived"),
            output_ordinal: 0,
            local_attachment_id: format!("{label:032x}"),
            blob: format!("{label:032x}"),
            preparation_generation: 1,
            byte_len: 7,
            sha256: sha256(b"derived"),
            name: name.into(),
            mime_type: "text/plain".into(),
            document_group: None,
            state: QueueDerivedFileState::Proposed,
        })
        .collect::<Vec<_>>();
        let preparation = QueueComposerPreparation::new(
            &owner(),
            seal,
            1,
            1,
            vec![QueuePreparationSource {
                identity: QueuePreparationSourceId::Source(0),
                byte_len: input.bytes().len() as u64,
                sha256: input.sha256(),
            }],
            files,
            Vec::new(),
        );
        snapshot
            .queue
            .plan_composer_preparation(
                &id,
                preparation,
                &snapshot.attachments,
                &mut snapshot.preparations,
                |_, plan| {
                    if plan.sources[0].sha256 != input.sha256()
                        || plan.files.iter().any(|file| {
                            source
                                .composer
                                .native
                                .draft
                                .text
                                .get(file.mention.start as usize..file.mention.end as usize)
                                != Some(file.mention.text.as_str())
                        })
                    {
                        return Err(JournalError::Invalid(
                            "test preparation differs from its exact explicit source mentions",
                        ));
                    }
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .expect("source-bound complete output plan");
        save(&mut journal, &snapshot);
        let first = journal
            .put(&snapshot.preparations[0], b"derived")
            .expect("actual first output bytes");
        snapshot
            .queue
            .publish_composer_derived_file(
                &id,
                1,
                0,
                &first,
                &mut snapshot.attachments,
                &mut snapshot.preparations,
                |_, _, _| Ok(()),
            )
            .expect("first output reaches catalog custody");
        save(&mut journal, &snapshot);
        let unpublished = journal
            .put(&snapshot.preparations[0], b"derived")
            .expect("second output completes before its publication is interrupted");
        let planned = snapshot
            .queue
            .operation(&id)
            .expect("original partial consumer")
            .composer_preparation()
            .expect("source-bound partial plan")
            .files[1]
            .descriptor()
            .expect("exact unpublished output descriptor");
        assert!(matches!(
            unpublished.commit,
            crate::attachment_spool::SpoolCommit::Durable
        ));
        assert_eq!(
            unpublished.descriptor, planned,
            "the actual second receipt stays held without granting catalog publication"
        );
        let descriptors = snapshot
            .queue
            .composer_file_descriptors(&id)
            .expect("exact catalog and unpublished obligations")
            .into_iter()
            .map(|file| {
                BlobDescriptor::new(
                    BlobId::parse(&file.id).expect("derived blob label"),
                    file.byte_len,
                    file.sha256,
                )
            })
            .collect();
        save(&mut journal, &snapshot);
        (id, descriptors)
    }

    fn worker_bytes(
        worker: &JournalWorker,
        snapshot: &JournalSnapshot,
        object: &QueueObjectId,
    ) -> Vec<u8> {
        let reference = snapshot
            .queue
            .object(object)
            .expect("retained immutable object")
            .reference();
        let mut bytes = Vec::new();
        for chunk in reference.chunks() {
            let part = worker
                .read_queue_chunk(chunk.descriptor().expect("held chunk"))
                .blocking_recv()
                .expect("worker read answers")
                .expect("verified private bytes");
            bytes.extend_from_slice(part.as_slice());
        }
        assert_eq!(sha256(&bytes), *reference.sha256());
        bytes
    }

    #[test]
    fn actual_short_frame_restart_reuses_expanded_inputs_and_original_identity_without_any_processor()
     {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("test-owned journal");
        let (mut snapshot, id, inputs) = fixture(&mut journal);
        let frozen = inputs
            .freeze(&client())
            .expect("original ordinary rich frame")
            .into_bytes();
        let (frame, capture) = plan(
            &mut snapshot,
            &id,
            "send",
            frozen.clone(),
            QueueObjectKind::FrozenSend,
            104,
        );
        let hold = snapshot
            .queue
            .object(&frame)
            .expect("original frame intent")
            .hold(0)
            .expect("unpublished original frame chunk");
        save(&mut journal, &snapshot);
        drop(journal);
        let path = profile.blob(
            hold.chunk
                .descriptor()
                .expect("exact local frame label")
                .id(),
        );
        let mut partial = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("actual private short file");
        partial
            .write_all(&frozen[..17])
            .expect("surviving prefix fixture");
        partial.sync_all().expect("durable short-file fixture");
        drop(partial);
        let inode = fs::metadata(&path).expect("original inode").ino();
        let (worker, loaded) = JournalWorker::start(profile.journal());
        let LoadedJournal::Present(mut restored) = loaded
            .blocking_recv()
            .expect("restart answers")
            .expect("restart loads")
        else {
            panic!("original local send remains captured");
        };
        let prepared = QueueObjectId::new(&format!("{}-prepared", id.as_str()))
            .expect("original prepared-input object");
        let inputs = PreparedInputs::decode(&worker_bytes(&worker, &restored, &prepared))
            .expect("read expanded captured inputs");
        let regenerated = inputs
            .freeze(&client())
            .expect("pure original-frame reconstruction")
            .into_bytes();
        assert_eq!(
            regenerated, frozen,
            "changed remote snippets cannot enter a reconstruction with no processor/client reads"
        );
        assert_eq!(capture.bytes(), regenerated);
        assert_eq!(
            worker
                .put_queue_chunk(hold.clone(), regenerated.clone())
                .blocking_recv()
                .expect("initial put answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Collision
            ))),
            "fresh activation has no repair authority"
        );
        let receipt = worker
            .resume_queue_chunk(hold.clone(), regenerated)
            .blocking_recv()
            .expect("explicit retry answers")
            .expect("exact prefix resumes");
        assert_eq!(fs::metadata(&path).expect("same frame file").ino(), inode);
        restored
            .queue
            .record_chunk(&hold, &receipt)
            .expect("same held ordinal");
        assert_eq!(
            worker
                .submit((*restored).clone())
                .blocking_recv()
                .expect("receipt save answers"),
            Ok(CommitOutcome::Durable)
        );
        let proof = worker
            .verify_queue_capture(
                restored
                    .queue
                    .object(&frame)
                    .expect("same original frame")
                    .verification(),
            )
            .blocking_recv()
            .expect("whole-frame verification answers")
            .expect("original bytes complete");
        restored
            .queue
            .record_verified(&proof)
            .expect("verified frozen capture");
        restored
            .queue
            .prepare_local_send(&id, frame, |_| Ok(()))
            .expect("ordinary Prepared transition");
        restored
            .queue
            .mark_send_unknown(&id, |_| Ok(()))
            .expect("ordinary Unknown transition");
        assert_eq!(
            worker
                .submit((*restored).clone())
                .blocking_recv()
                .expect("dispatch barrier answers"),
            Ok(CommitOutcome::Durable)
        );
        let retained = restored
            .queue
            .operation(&id)
            .expect("same original operation");
        assert!(
            restore_frame(
                &client(),
                retained,
                &inputs,
                fs::read(&path).expect("complete exact frame")
            )
            .is_ok()
        );
        assert!(
            worker
                .authorize_queue_acknowledgement(id.clone(), Vec::new())
                .blocking_recv()
                .expect("acknowledgement check answers")
                .is_err(),
            "an ordinary local capture never gains Take acknowledgement authority"
        );
        assert_eq!(retained.id(), &id);
        assert_eq!(
            worker.finish().blocking_recv().expect("worker drains"),
            Ok(CommitOutcome::Durable)
        );
    }

    #[test]
    fn actual_retirement_continuation_survives_consumer_removal_without_source_objects() {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("test-owned journal");
        let (mut snapshot, id, inputs) = fixture(&mut journal);
        let (frame, capture) = plan(
            &mut snapshot,
            &id,
            "send",
            inputs
                .freeze(&client())
                .expect("original frozen frame")
                .into_bytes(),
            QueueObjectKind::FrozenSend,
            104,
        );
        publish(&mut journal, &mut snapshot, &frame, capture, false);
        snapshot
            .queue
            .prepare_local_send(&id, frame, |_| Ok(()))
            .expect("ordinary Prepared transition");
        snapshot.drafts.clear();
        snapshot
            .queue
            .record_send_admitted(&id, &MessageId::from(source().intent.id), |_| Ok(()))
            .expect("exact admitted consumer");
        let files = source().intent.files;
        snapshot
            .queue
            .begin_local_send_retirement(&id, files.clone(), |_| Ok(()))
            .expect("typed cleanup continuation");
        save(&mut journal, &snapshot);
        snapshot
            .queue
            .retire_operation(&id, |_| Ok(()))
            .expect("consumer may retire after its continuation commits");
        save(&mut journal, &snapshot);
        drop(journal);

        let (mut journal, loaded) =
            AttachmentJournal::open(&profile.journal()).expect("restart with no source consumer");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("retirement continuation survives restart");
        };
        let mut restored = *restored;
        assert!(restored.queue.operation(&id).is_none());
        let retirement = restored
            .queue
            .local_send_retirement(&id)
            .expect("consumer removal cannot lose local cleanup");
        assert_eq!(
            retirement
                .owner
                .draft_key()
                .expect("captured original owner"),
            owner()
        );
        assert_eq!(retirement.files[0].id, files[0].id);
        assert_eq!(retirement.files[0].byte_len, files[0].byte_len);
        assert_eq!(retirement.files[0].sha256, files[0].sha256);
        assert!(retirement.dependencies.is_empty());
        let mut state = LocalSendState::default();
        state.restore(&restored.queue);
        assert_eq!(state.check(&id), LocalSendCheck::Admitted);
        assert!(
            !state.any_busy(),
            "hydration projects the continuation without executing cleanup"
        );

        let attachment = AttachmentId::from(files[0].id.clone());
        let blob = BlobId::parse(&files[0].id).expect("captured file retirement identity");
        assert!(matches!(
            crate::attachment_retirement::begin_retirement(
                &mut restored.attachments,
                &mut restored.retired_blobs,
                &attachment,
                blob
            ),
            crate::attachment_retirement::RetirementStart::Ready
        ));
        save(&mut journal, &restored);
        restored
            .queue
            .finish_local_send_retirement(&id, |_| Ok(()))
            .expect("durable attachment intent takes over local byte cleanup");
        save(&mut journal, &restored);
        assert!(restored.queue.local_send_retirement(&id).is_none());
        assert!(
            !restored
                .queue
                .retired_blobs()
                .expect("independent queue chunk retirement")
                .is_empty()
        );
        assert!(restored.retired_blobs.contains(&blob));
        assert!(
            profile.blob(&blob).is_file(),
            "named byte-release intents remain available to explicit storage retry"
        );
    }

    #[test]
    fn a_short_published_attachment_is_not_replaced_or_read_from_a_model_facing_filename() {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("test-owned journal");
        let (snapshot, _, inputs) = fixture(&mut journal);
        let descriptor = inputs.header.sources[0].files[0]
            .descriptor()
            .expect("published file descriptor");
        drop(journal);
        let path = profile.blob(descriptor.id());
        let inode = fs::metadata(&path).expect("owned attachment inode").ino();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("test-owned published file");
        file.set_len(3).expect("short-file fixture only");
        drop(file);
        let (worker, loaded) = JournalWorker::start(profile.journal());
        assert!(
            loaded.blocking_recv().expect("restart answers").is_ok(),
            "hydration does not inspect attachment bytes"
        );
        assert!(
            worker
                .read_attachment(descriptor)
                .blocking_recv()
                .expect("explicit held-byte read answers")
                .is_err()
        );
        assert_eq!(fs::metadata(&path).expect("unchanged inode").ino(), inode);
        assert_eq!(fs::read(&path).expect("unchanged short artifact"), b"ori");
        assert!(snapshot.attachments.contains_key(descriptor.id().as_str()));
        assert_eq!(
            worker.finish().blocking_recv().expect("worker drains"),
            Ok(CommitOutcome::Durable)
        );
    }
}
