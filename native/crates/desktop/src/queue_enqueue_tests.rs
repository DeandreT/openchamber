use super::*;

use openchamber_native_core::draft::Draft;
use openchamber_native_core::draft_content::{AgentMention, DraftContext, DraftFile, SkillRef};
use openchamber_native_core::ids::{AttachmentId, ContextItemId};
use openchamber_native_transport::prompt_parts::Metadata;
use openchamber_native_transport::queue_manifest::{
    QueueFileReference, QueueLocalFile, QueueLocalItem, QueuePayloadItem,
};

use crate::draft_companion::CompanionCapture;
use crate::queue_capture::QueueObjectId;
use crate::queue_storage::{ComposerEditRef, DraftCompanionRef};

fn configuration() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "model".into(),
        agent: Some("build".into()),
        variant: Some("effort".into()),
    }
}

fn native() -> QueueNativeSource {
    let owner = DraftKey::new(
        RuntimeId::from("runtime"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_queue")),
    )
    .expect("exact native owner");
    let mut draft = Draft::new("\n@build keep λ 👩\n");
    draft.content.agents.push(AgentMention {
        name: "build".into(),
        mention: Some(Mention {
            start: 1,
            end: 7,
            text: "@build".into(),
        }),
    });
    draft.content.skills.push(SkillRef {
        id: "skill_original".into(),
    });
    draft.content.metadata_json = Some(r#"{"keep":{"array":[1,true,"λ"]}}"#.into());
    draft.content.context.push(DraftContext {
        id: ContextItemId::from("native-chip"),
        text: "original context".into(),
        description: Some("original description".into()),
        metadata_json: Some(r#"{"source":"fixture"}"#.into()),
        instructions: Some("instructions first".into()),
    });
    draft.content.files.push(DraftFile {
        id: AttachmentId::from("00000000000000000000000000000001"),
        name: Some("original.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(8),
        source_document_id: Some(AttachmentId::from("source-group")),
        content: DraftFileContent::Prepared,
    });
    QueueNativeSource { owner, draft }
}

fn file() -> QueueAttachmentDescriptor {
    QueueAttachmentDescriptor {
        blob_id: [1; 32],
        request_id: "original_request".into(),
        attachment_id: "file-0".into(),
        filename: "original.txt".into(),
        mime_type: "text/plain".into(),
        size: 8,
        sha256: sha256(b"original"),
    }
}

#[test]
fn enqueue_keeps_complete_source_and_frozen_configuration_without_reparsing_text() {
    let source = native();
    let captured =
        manifest(&source, configuration(), vec![file()]).expect("complete native conversion");
    assert_eq!(
        captured.content, source.draft.text,
        "typed whitespace and Unicode remain exact"
    );
    assert_eq!(
        captured.text, source.draft.text,
        "enqueue does not re-resolve mentions from mutable catalogs"
    );
    assert_eq!(captured.send_config, configuration());
    assert_eq!(captured.attachment_manifest[0].attachment, file());
    assert_eq!(
        captured.prompt_parts.agents[0]
            .mention
            .as_ref()
            .expect("mention")
            .start,
        1
    );
    assert_eq!(captured.prompt_parts.skills[0].id, "skill_original");
    assert_eq!(captured.prompt_parts.context[0].text, "instructions first");
    assert_eq!(captured.prompt_parts.context[1].text, "original context");
    assert_eq!(
        captured.prompt_parts.context[1].description.as_deref(),
        Some("original description")
    );
    assert!(
        captured
            .prompt_parts
            .context
            .iter()
            .all(|context| context.id.is_none()),
        "only the immutable enqueue receipt assigns admission IDs to native semantic chips"
    );
    let document = QueuePayloadDocument::from_manifest(
        "/project",
        captured.clone(),
        QueuePayloadUse::Enqueue,
        sha256,
    )
    .expect("strict payload encoding");
    assert_eq!(document.item(), &QueuePayloadItem::Manifest(captured));
    assert!(
        document.files_proof(sha256).is_err(),
        "enqueue input grants no take acknowledgement proof"
    );
    let restored: QueueNativeSource =
        serde_json::from_slice(&serde_json::to_vec(&source).expect("native companion"))
            .expect("lossless source companion");
    assert_eq!(
        restored.draft, source.draft,
        "native grouping and original semantic IDs survive independently of protocol parts"
    );
}

#[test]
fn missing_file_or_unrepresentable_context_is_refused_without_consuming_the_source() {
    let source = native();
    let before = source.draft.clone();
    assert!(manifest(&source, configuration(), Vec::new()).is_err());
    assert_eq!(source.draft, before);
    let mut source = source;
    source.draft.content.context[0].text.clear();
    assert!(
        manifest(&source, configuration(), vec![file()]).is_err(),
        "description and metadata cannot disappear with a blank context body"
    );
}

#[test]
fn server_runtime_hold_is_independent_of_projection_generation() {
    let mut state = EnqueueState::default();
    let id = QueueOperationId::new("original_operation").expect("operation");
    state.tasks.insert(
        id.clone(),
        EnqueueTask {
            runtime: RuntimeId::from("original-host"),
            owner: None,
            native_roots: Vec::new(),
            _task: Task::ready(()),
        },
    );
    assert!(state.holds_runtime(&RuntimeId::from("original-host")));
    assert!(!state.holds_runtime(&RuntimeId::from("replacement-host")));
    assert!(state.any_busy());
    assert!(state.busy(&id));
    assert!(
        !state.busy(&QueueOperationId::new("independent_operation").expect("another identity")),
        "one operation does not disable another owner's recovery"
    );
    state.tasks.remove(&id);
    assert!(!state.holds_runtime(&RuntimeId::from("original-host")));
}

#[test]
fn native_root_close_holds_include_foreign_runtime_tasks_and_exclude_rootless_work() {
    let mut state = EnqueueState::default();
    let rootless = QueueOperationId::new("rootless_recovery").expect("rootless operation");
    state.tasks.insert(
        rootless,
        EnqueueTask {
            runtime: RuntimeId::from("active-host"),
            owner: None,
            native_roots: Vec::new(),
            _task: Task::ready(()),
        },
    );
    assert!(
        state.any_busy() && !state.has_native_root_holds(),
        "background recovery without native roots cannot delay close"
    );
    let rooted = QueueOperationId::new("captured_original_host").expect("original operation");
    let file = AttachmentId::from("00000000000000000000000000000001");
    state.tasks.insert(
        rooted.clone(),
        EnqueueTask {
            runtime: RuntimeId::from("original-host"),
            owner: None,
            native_roots: vec![file],
            _task: Task::ready(()),
        },
    );
    assert!(
        state.has_native_root_holds(),
        "a window retains captured roots independently of the selected runtime or composer owner"
    );
    state
        .tasks
        .get_mut(&rooted)
        .expect("rooted actor")
        .native_roots
        .clear();
    assert!(
        state.any_busy() && !state.has_native_root_holds(),
        "ledger handoff releases the close hold while the actor remains live"
    );
}

fn original_file_descriptor() -> QueueNativeFileDescriptor {
    QueueNativeFileDescriptor {
        id: native().draft.content.files[0].id.to_string(),
        byte_len: 8,
        sha256: sha256(b"original"),
    }
}

fn typed_enqueue() -> QueueOperation {
    let native = native();
    let reference = DraftCompanionRef {
        operation: QueueOperationId::new("original_take").expect("take identity"),
        companion: QueueObjectId::new("original_take-destination").expect("companion identity"),
    };
    let edit = ComposerEditRef {
        operation: reference.operation.clone(),
        object: QueueObjectId::new("original_take-edit").expect("editing identity"),
        generation: 7,
        revision: 19,
        byte_len: 88,
        sha256: [9; 32],
        companions: vec![reference.clone()],
    };
    let owner = QueueOwner::new(
        native.owner.runtime(),
        "/project",
        &SessionId::from("ses_queue"),
        &native.owner,
    )
    .expect("captured owner");
    let mut operation = QueueOperation::enqueue_with_edit(
        QueueOperationId::new("compound_enqueue").expect("operation"),
        owner,
        QueueConfiguration::capture(&configuration()),
        CompanionCapture {
            native,
            references: vec![reference],
        },
        vec![
            "msg_native_instructions".into(),
            "msg_native_context".into(),
        ],
        vec![original_file_descriptor()],
        Some(edit),
    )
    .expect("original compound capture");
    operation
        .begin_composer_preparation(7, 19)
        .expect("marker is part of initial admission");
    operation
}

#[test]
fn compound_source_and_initial_marker_preserve_the_exact_original_edit_and_byte_authority() {
    let operation = typed_enqueue();
    let source = operation
        .enqueue_capture()
        .expect("typed original source accessor");
    validate_compound_source(&operation, &source)
        .expect("original source matches its immutable seal");
    let before = serde_json::to_vec(&source).expect("original source bytes");
    assert_eq!(source.composer.native.draft, native().draft);
    assert!(source.intent.files == [original_file_descriptor()]);
    assert!(source.intent.edit.as_ref() == operation.composer_edit());
    assert_eq!(source.composer.references, operation.dependencies());
    let plan = operation
        .composer_preparation()
        .expect("preparation marker survives before the first read");
    assert_eq!((plan.generation, plan.revision), (7, 19));
    assert!(
        plan.sources.is_empty() && plan.files.is_empty() && plan.derived_context_ids.is_empty()
    );
    assert!(
        plan.inputs_object().is_none(),
        "the initial marker grants no prepared input authority"
    );
    let restored: QueueOperation =
        serde_json::from_slice(&serde_json::to_vec(&operation).expect("journal operation"))
            .expect("same operation after restart");
    let restored_source = restored
        .enqueue_capture()
        .expect("original source after restart");
    assert_eq!(
        before,
        serde_json::to_vec(&restored_source).expect("same canonical original source")
    );
    assert!(restored.composer_preparation().is_some());
    let mut changed = restored_source;
    changed.composer.native.draft.text.push_str("new typing");
    assert!(
        validate_compound_source(&restored, &changed).is_err(),
        "new composer input cannot replace an admitted original source"
    );
    assert_eq!(
        before,
        serde_json::to_vec(&source).expect("original remains immutable")
    );
}

#[test]
fn pristine_unified_markers_pass_canonical_admission_for_send_enqueue_and_prefix() {
    use crate::queue_storage::{NativePrefixIntent, NativePrefixKind, QueueStorage};

    let mut native = native();
    native.draft = Draft::new("ordinary native text");
    let owner = QueueOwner::new(
        native.owner.runtime(),
        "/project",
        &SessionId::from("ses_queue"),
        &native.owner,
    )
    .expect("captured owner");
    let capture = CompanionCapture {
        native: native.clone(),
        references: Vec::new(),
    };
    let prefix = NativePrefixIntent {
        kind: NativePrefixKind::Capture,
        directory: "/project".into(),
        id: "msg_marker_prefix".into(),
        item_ids: vec!["queued_original".into()],
        delivery: None,
        companions: Vec::new(),
        native_context_ids: Vec::new(),
        files: Vec::new(),
        edit: None,
    };
    let operations = [
        QueueOperation::local_send(
            QueueOperationId::new("marker_send").expect("send ID"),
            owner.clone(),
            QueueConfiguration::capture(&configuration()),
            MessageId::from("msg_marker_send"),
            capture.clone(),
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("native send"),
        QueueOperation::enqueue_with_edit(
            QueueOperationId::new("marker_enqueue").expect("enqueue ID"),
            owner.clone(),
            QueueConfiguration::capture(&configuration()),
            capture,
            Vec::new(),
            Vec::new(),
            None,
        )
        .expect("native enqueue"),
        QueueOperation::prefix(
            QueueOperationId::new("marker_prefix").expect("prefix ID"),
            owner,
            QueueConfiguration::capture(&configuration()),
            MessageId::from("msg_marker_prefix"),
            native,
            QueueJson::new(serde_json::to_vec(&prefix).expect("prefix intent"))
                .expect("bounded intent"),
        )
        .expect("native prefix"),
    ];
    for mut operation in operations {
        operation
            .begin_composer_preparation(1, 1)
            .expect("real initial marker");
        let original = serde_json::to_vec(&operation).expect("original marked operation");
        let mut queue = QueueStorage::default();
        queue
            .admit_operation(operation, |candidate| {
                candidate.validate(&Default::default(), &[], &[], &mut HashSet::new())
            })
            .map_err(|(error, _)| error)
            .expect("the pristine marker must pass the actual canonical admission validator");
        let restored: QueueStorage =
            serde_json::from_slice(&serde_json::to_vec(&queue).expect("durable ledger"))
                .expect("restart ledger");
        restored
            .validate(&Default::default(), &[], &[], &mut HashSet::new())
            .expect("the pristine marker remains valid after restart");
        assert_eq!(
            serde_json::to_vec(&restored.operations()[0]).expect("same operation"),
            original
        );
        let mut changed: serde_json::Value =
            serde_json::from_slice(&original).expect("marked operation object");
        changed["preparation"]["derived_context_ids"] = serde_json::json!(["msg_without_source"]);
        let invalid =
            serde_json::from_value(changed).expect("well-shaped but unproven derived context");
        let mut refused = QueueStorage::default();
        assert!(
            refused
                .admit_operation(invalid, |candidate| candidate.validate(
                    &Default::default(),
                    &[],
                    &[],
                    &mut HashSet::new()
                ))
                .is_err(),
            "an empty marker cannot grant derived context authority"
        );
        assert!(
            refused.operations().is_empty(),
            "refusal rolls back the candidate"
        );
    }
}

fn projected_capture() -> (QueueLocalCapture, Vec<BlobDescriptor>) {
    use crate::composer_delivery::{ComposerDelivery, ComposerDeliverySource};

    let context = |id, text: &str| QueuePromptContext {
        id: Some(MessageId::from(id)),
        text: text.into(),
        description: Some("retained description".into()),
        metadata: Some(Metadata(
            serde_json::from_value(serde_json::json!({"source":text})).expect("metadata"),
        )),
    };
    let projected = "λ | 👩 @build | expanded original";
    let anchor = QueueMention {
        start: 7,
        end: 13,
        text: "@build".into(),
    };
    let native_context = context("msg_native", "native context");
    let skill_context = context("msg_skill", "captured unresolved skill instruction");
    let companion_context = context("msg_companion", "original companion context");
    let native = QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: "native".into(),
            text: projected.into(),
            files: vec![QueueLocalFile {
                name: "original.txt".into(),
                mime_type: "text/plain".into(),
                byte_len: 8,
                sha256: sha256(b"original"),
                description: Some("original file description".into()),
                mention: Some(anchor.clone()),
            }],
            send_config: configuration(),
            prompt_parts: QueuePromptParts {
                agents: vec![QueueAgent {
                    name: "build".into(),
                    mention: Some(anchor.clone()),
                }],
                context: vec![native_context, skill_context.clone()],
                metadata: Some(Metadata(
                    serde_json::from_value(serde_json::json!({"native":{"keep":true}}))
                        .expect("native metadata"),
                )),
                ..QueuePromptParts::default()
            },
        },
        vec![MessageId::from("msg_native"), MessageId::from("msg_skill")],
        sha256,
    )
    .expect("projected native source");
    let companion = QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: "#snippet".into(),
            text: String::new(),
            files: Vec::new(),
            send_config: configuration(),
            prompt_parts: QueuePromptParts {
                file_references: vec![QueueFileReference {
                    uri: "file:///project/notes.pdf".into(),
                    name: "notes.pdf".into(),
                    description: Some("reference without owned bytes".into()),
                    mention: Some(anchor.clone()),
                }],
                skills: vec![QueueSkill {
                    id: "original-skill".into(),
                    mention: Some(anchor),
                }],
                context: vec![companion_context],
                metadata: Some(Metadata(
                    serde_json::from_value(serde_json::json!({"companion":[1,true,"λ"]}))
                        .expect("companion metadata"),
                )),
                ..QueuePromptParts::default()
            },
        },
        vec![MessageId::from("msg_companion")],
        sha256,
    )
    .expect("edited companion retains original context");
    let held = original_file_descriptor();
    let delivery = ComposerDelivery {
        sources: vec![
            ComposerDeliverySource {
                capture: native,
                files: vec![BlobDescriptor::new(
                    BlobId::parse(&held.id).expect("original blob"),
                    held.byte_len,
                    held.sha256,
                )],
                original: None,
            },
            ComposerDeliverySource {
                capture: companion,
                files: Vec::new(),
                original: None,
            },
        ],
        content: "λ | 👩 @build | #snippet".into(),
        text: projected.into(),
        context_ids: vec![
            MessageId::from("msg_native"),
            MessageId::from("msg_skill"),
            MessageId::from("msg_companion"),
        ],
        configuration: configuration(),
        derived_contexts: vec![skill_context],
    };
    delivery
        .into_capture()
        .expect("shared merged delivery preserves global anchors")
}

#[test]
fn compound_enqueue_preserves_exact_display_projection_global_anchors_all_parts_and_context_order()
{
    let (capture, held) = projected_capture();
    let manifest = compound_manifest(&capture, vec![file()])
        .expect("uploaded bytes replace only local byte descriptors");
    assert_eq!(manifest.content, "λ | 👩 @build | #snippet");
    assert_eq!(manifest.text, "λ | 👩 @build | expanded original");
    assert_eq!(manifest.send_config, configuration());
    assert_eq!(
        manifest.attachment_manifest[0].description.as_deref(),
        Some("original file description")
    );
    assert_eq!(
        manifest.prompt_parts.file_references[0].uri,
        "file:///project/notes.pdf"
    );
    assert_eq!(
        manifest.prompt_parts.file_references[0]
            .description
            .as_deref(),
        Some("reference without owned bytes")
    );
    for anchor in [
        &manifest.attachment_manifest[0].mention,
        &manifest.prompt_parts.file_references[0].mention,
        &manifest.prompt_parts.agents[0].mention,
        &manifest.prompt_parts.skills[0].mention,
    ] {
        assert_eq!(
            anchor.as_ref().map(|anchor| (anchor.start, anchor.end)),
            Some((7, 13)),
            "projected anchors never shift twice at enqueue"
        );
    }
    assert_eq!(
        manifest
            .prompt_parts
            .context
            .iter()
            .map(|context| context.id.clone().expect("original identity"))
            .collect::<Vec<_>>(),
        capture.context_ids()
    );
    assert_eq!(
        capture.context_ids(),
        [
            MessageId::from("msg_native"),
            MessageId::from("msg_skill"),
            MessageId::from("msg_companion")
        ]
    );
    let metadata = &manifest
        .prompt_parts
        .metadata
        .as_ref()
        .expect("all source metadata")
        .0;
    assert!(metadata.contains_key("native") && metadata.contains_key("companion"));
    let document =
        QueuePayloadDocument::from_manifest("/project", manifest, QueuePayloadUse::Enqueue, sha256)
            .expect("existing queue format accepts full compound payload");
    validate_compound_document(&document, &capture)
        .expect("final payload matches its sealed prepared input");
    assert!(
        document.files_proof(sha256).is_err(),
        "enqueue payloads grant no invented transfer index or acknowledgement proof"
    );
    let prepared = PreparedComposerInput::new(capture, held, 7, 19, [3; 32])
        .expect("immutable expanded input");
    let bytes = prepared.encode().expect("durable expanded input encoding");
    let restored = PreparedComposerInput::decode(&bytes, "/project")
        .expect("original prepared bytes restore without reprocessing");
    assert_eq!(restored.capture.bytes(), prepared.capture.bytes());
    assert_eq!(restored.files, prepared.files);
    assert_eq!(restored.encode().expect("same immutable inputs"), bytes);
}

#[test]
fn reference_only_compound_enqueue_never_substitutes_or_requests_attachment_bytes() {
    let capture = QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: String::new(),
            text: String::new(),
            files: Vec::new(),
            send_config: configuration(),
            prompt_parts: QueuePromptParts {
                file_references: vec![QueueFileReference {
                    uri: "file:///project/source.pdf".into(),
                    name: "source.pdf".into(),
                    description: Some("original reference".into()),
                    mention: None,
                }],
                ..QueuePromptParts::default()
            },
        },
        Vec::new(),
        sha256,
    )
    .expect("real typed reference-only source");
    let manifest =
        compound_manifest(&capture, Vec::new()).expect("reference-only enqueue needs no bytes");
    assert!(manifest.attachment_manifest.is_empty());
    assert_eq!(
        manifest.prompt_parts.file_references,
        capture.item().prompt_parts.file_references
    );
    let document =
        QueuePayloadDocument::from_manifest("/project", manifest, QueuePayloadUse::Enqueue, sha256)
            .expect("reference-only existing protocol");
    assert!(document.files_proof(sha256).is_err());
    assert!(
        compound_manifest(&capture, vec![file()]).is_err(),
        "a reference URI cannot become a fake owned-byte binding"
    );
}

#[test]
fn compound_payload_restoration_rejects_changed_delivery_or_file_descriptors() {
    let (capture, _) = projected_capture();
    let mut wrong = file();
    wrong.sha256 = sha256(b"replaced");
    assert!(compound_manifest(&capture, vec![wrong]).is_err());
    let mut changed = compound_manifest(&capture, vec![file()]).expect("original manifest");
    changed.text.push_str(" changed after restart");
    let document =
        QueuePayloadDocument::from_manifest("/project", changed, QueuePayloadUse::Enqueue, sha256)
            .expect("independently valid different payload");
    assert!(
        validate_compound_document(&document, &capture).is_err(),
        "an independently valid payload cannot replace the sealed prepared inputs"
    );
}

#[test]
fn compound_manifest_preserves_exact_model_identity_separately_from_safe_upload_names() {
    let (original, _) = projected_capture();
    let original_bytes = original.bytes().to_vec();
    let names = [
        ("notes.txt".to_owned(), "notes.txt"),
        ("/original/source/notes.txt".to_owned(), "notes.txt"),
        (r"C:\original\notes.txt".to_owned(), "notes.txt"),
        (r"\\server\share\notes.txt".to_owned(), "notes.txt"),
        ("relative/source/notes.txt".to_owned(), "notes.txt"),
        (r"literal\posix-name.txt".to_owned(), "file-0.bin"),
        ("/original/tab\tname.txt".to_owned(), "file-0.bin"),
        ("x".repeat(4097), "file-0.bin"),
    ];
    for (name, upload_name) in names {
        let mut item = original.item().clone();
        item.files[0].name = name.clone();
        let capture =
            QueueLocalCapture::new("/project", item, original.context_ids().to_vec(), sha256)
                .expect("existing model-name vocabulary");
        let mut uploaded = file();
        uploaded.filename = upload_name.into();
        let manifest = compound_manifest(&capture, vec![uploaded])
            .expect("exact prepared bytes with a safe upload label");
        assert_eq!(
            manifest.attachment_manifest[0].attachment.filename,
            upload_name
        );
        assert_eq!(
            manifest.attachment_manifest[0].agent_path.as_deref(),
            (name != upload_name).then_some(name.as_str())
        );
        let mut changed = manifest.clone();
        changed.attachment_manifest[0].agent_path = Some("/changed/model/identity.txt".into());
        let changed = QueuePayloadDocument::from_manifest(
            "/project",
            changed,
            QueuePayloadUse::Enqueue,
            sha256,
        )
        .expect("individually valid changed model identity");
        assert!(
            validate_compound_document(&changed, &capture).is_err(),
            "matching byte descriptors cannot hide a changed model identity"
        );
        let document = QueuePayloadDocument::from_manifest(
            "/project",
            manifest,
            QueuePayloadUse::Enqueue,
            sha256,
        )
        .expect("lossless typed enqueue");
        validate_compound_document(&document, &capture)
            .expect("the uploaded manifest matches captured identity and bytes");
        assert!(
            document.files_proof(sha256).is_err(),
            "a new manifest still grants no transfer acknowledgement proof"
        );
        let edited =
            QueueLocalCapture::from_document(document, capture.context_ids().to_vec(), sha256)
                .expect("explicit local edit conversion");
        assert_eq!(
            edited.item().files[0].name,
            name,
            "editing never substitutes the temporary upload label"
        );
        assert_eq!(edited.item().files[0].sha256, sha256(b"original"));
        assert_eq!(
            original.bytes(),
            original_bytes,
            "the original compound capture remains immutable"
        );
    }
}

#[test]
fn legacy_v8_native_enqueue_defaults_keep_their_original_recovery_path() {
    let native = native();
    let owner = QueueOwner::new(
        native.owner.runtime(),
        "/project",
        &SessionId::from("ses_queue"),
        &native.owner,
    )
    .expect("original owner");
    let operation = QueueOperation::enqueue(
        QueueOperationId::new("legacy_enqueue").expect("legacy identity"),
        owner,
        QueueConfiguration::capture(&configuration()),
        native.clone(),
    )
    .expect("legacy native-only operation");
    let mut older = serde_json::to_value(operation).expect("version-8 native shape");
    for field in [
        "source_intent",
        "source_seal",
        "preparation",
        "prefix_downloads",
    ] {
        older
            .as_object_mut()
            .expect("operation object")
            .remove(field);
    }
    let restored: QueueOperation =
        serde_json::from_value(older).expect("older version-8 defaults remain compatible");
    assert!(!restored.has_compound_source());
    assert!(restored.composer_preparation().is_none() && restored.source_sha256().is_none());
    assert!(
        restored.enqueue_capture().is_err(),
        "legacy recovery does not guess a typed compound intent"
    );
    let QueueAction::Enqueue {
        source: QueueSource::Inline(original),
        ..
    } = restored.action()
    else {
        panic!("legacy source remains inline");
    };
    assert_eq!(original.draft, native.draft);
}

#[gpui_kit::test]
fn initial_enqueue_actor_blocks_only_its_captured_owner_and_protects_native_roots(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::AppContext;
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |workspace, _| {
            let native = native();
            let id =
                QueueOperationId::new("pre_admission_enqueue").expect("captured actor identity");
            let file = native.draft.content.files[0].id.clone();
            let group = native.draft.content.files[0]
                .source_document_id
                .clone()
                .expect("source-document group");
            workspace.queue.enqueues.tasks.insert(
                id.clone(),
                EnqueueTask {
                    runtime: native.owner.runtime().clone(),
                    owner: Some(native.owner.clone()),
                    native_roots: vec![file.clone(), group.clone()],
                    _task: Task::ready(()),
                },
            );
            assert!(
                workspace.queue_storage.operation(&id).is_none(),
                "the actor protects capture while its editing flush precedes admission"
            );
            assert!(workspace.enqueue_blocks_owner(&native.owner));
            assert!(
                workspace
                    .queue
                    .enqueues
                    .blocks_session(native.owner.runtime(), &SessionId::from("ses_queue"))
            );
            assert!(
                !workspace
                    .queue
                    .enqueues
                    .blocks_session(native.owner.runtime(), &SessionId::from("ses_other"))
            );
            assert!(
                workspace.queue.enqueues.references_native(&file)
                    && workspace.queue.enqueues.references_native(&group)
            );
            let other = DraftKey::new(
                native.owner.runtime().clone(),
                "/project",
                DraftTarget::Session(SessionId::from("ses_other")),
            )
            .expect("independent owner");
            assert!(!workspace.enqueue_blocks_owner(&other));
            workspace.runtime = RuntimeId::from("replacement-runtime");
            assert!(
                workspace
                    .queue
                    .enqueues
                    .holds_runtime(native.owner.runtime())
            );
            workspace.queue.enqueues.tasks.remove(&id);
            assert!(!workspace.enqueue_blocks_owner(&native.owner));
        })
    })
    .expect("captured owner remains independent of the selected runtime");
}

#[cfg(unix)]
mod compound_workspace {
    use super::*;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, Entity, EntityInputHandler, TestAppContext};
    use openchamber_native_transport::queue_manifest::QueueIndexedPayload;

    use crate::attachment_io::AttachmentJournal;
    use crate::draft_companion::CompanionDocument;
    use crate::journal_worker::CommitOutcome;
    use crate::mutation_journal::{JournalCommit, JournalSnapshot};
    use crate::queue_capture::QueueCaptureBytes;
    use crate::queue_storage::{
        QueueComposerPreparation, QueueDestination, QueueObject, QueuePreparationSource,
        QueuePreparationSourceId, QueueTransfer, QueueTransferItem,
    };

    fn json(value: serde_json::Value) -> QueueJson {
        QueueJson::new(serde_json::to_vec(&value).expect("fixture JSON")).expect("bounded metadata")
    }

    fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
        assert!(
            matches!(journal.save(snapshot), Ok(JournalCommit::Durable)),
            "the whole original snapshot is durable"
        );
    }

    fn publish(
        journal: &mut AttachmentJournal,
        snapshot: &mut JournalSnapshot,
        operation: &QueueOperationId,
        role: &str,
        bytes: Vec<u8>,
        kind: QueueObjectKind,
        label: u128,
    ) -> QueueObjectId {
        let capture = QueueCaptureBytes::new(
            kind,
            bytes,
            &[BlobId::parse(&format!("{label:032x}")).expect("private label")],
        )
        .expect("small complete capture");
        let id = QueueObjectId::new(&format!("{}-{role}", operation.as_str()))
            .expect("stable object role");
        assert!(
            snapshot
                .queue
                .admit_object(
                    QueueObject::planned(
                        id.clone(),
                        operation.clone(),
                        kind,
                        1,
                        capture.reference().clone()
                    )
                    .expect("pre-put hold"),
                    |_| Ok(())
                )
                .is_ok()
        );
        save(journal, snapshot);
        let hold = snapshot
            .queue
            .object(&id)
            .expect("captured object")
            .hold(0)
            .expect("unpublished exact hold");
        let receipt = journal
            .put_queue(&hold, capture.bytes())
            .expect("private spool write");
        snapshot
            .queue
            .record_chunk(&hold, &receipt)
            .expect("durable chunk receipt");
        save(journal, snapshot);
        let proof = journal
            .verify_queue(
                &snapshot
                    .queue
                    .object(&id)
                    .expect("same object")
                    .verification(),
            )
            .expect("whole-stream byte proof");
        snapshot
            .queue
            .record_verified(&proof)
            .expect("retain actual verification");
        save(journal, snapshot);
        id
    }

    fn seed_companion(journal: &mut AttachmentJournal, native_text: &str) -> DraftKey {
        let mut original = native();
        original.draft = Draft::new(native_text);
        let owner = original.owner.clone();
        let operation = QueueOperationId::new("edited_take_original").expect("original Take");
        let context = QueuePromptContext {
            id: Some(MessageId::from("msg_original")),
            text: "original context".into(),
            description: Some("original description".into()),
            metadata: None,
        };
        let document = QueuePayloadDocument::from_manifest(
            "/project",
            QueueManifestItem {
                content: "#snippet".into(),
                text: "original expansion".into(),
                attachment_manifest: Vec::new(),
                context: Vec::new(),
                context_preview: None,
                send_config: configuration(),
                prompt_parts: QueuePromptParts {
                    file_references: vec![QueueFileReference {
                        uri: "file:///project/reference.pdf".into(),
                        name: "reference.pdf".into(),
                        description: Some("reference only".into()),
                        mention: None,
                    }],
                    agents: vec![QueueAgent {
                        name: "build".into(),
                        mention: None,
                    }],
                    skills: vec![QueueSkill {
                        id: "captured-skill".into(),
                        mention: None,
                    }],
                    context: vec![context],
                    metadata: Some(Metadata(
                        serde_json::from_value(serde_json::json!({"original":true}))
                            .expect("original metadata"),
                    )),
                },
            },
            QueuePayloadUse::Enqueue,
            sha256,
        )
        .expect("real original rich queue document");
        let digest = |bytes: &[u8]| {
            sha256(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let hash = digest(document.bytes());
        let length = document.bytes().len();
        let pointer = format!(
            r#"{{"version":1,"encoding":"utf-8-json","totalBytes":{length},"sha256":"{hash}","chunks":[{{"attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":{length},"sha256":"{hash}","blobId":"{}","requestId":"upload_original","ordinal":0}}]}}"#,
            "cd".repeat(32)
        );
        let index = r##"{"directory":"/project","sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"effort"},"preview":"#snippet","entryCount":4,"contextIds":["msg_original"],"attachmentBlobIds":[],"promptBytes":512,"admissionBytes":1024,"structuredBytes":0}"##;
        let seal =
            digest(format!(r#"{{"payloadManifest":{pointer},"payloadIndex":{index}}}"#).as_bytes());
        let indexed = QueueIndexedPayload::from_body(format!(r#"{{"format":"payload-v1","payloadManifest":{pointer},"payloadIndex":{index},"payloadIndexHash":"{seal}"}}"#).as_bytes(), "/project", sha256)
            .expect("real schema-ordered original index");
        let document = indexed
            .materialize(document.bytes().to_vec(), QueuePayloadUse::Enqueue, sha256)
            .expect("original document verifies against its sealed index");
        let files = document
            .files_proof(sha256)
            .expect("actual original materialization proof");
        let queue_owner = QueueOwner::new(
            owner.runtime(),
            "/project",
            &SessionId::from("ses_queue"),
            &owner,
        )
        .expect("captured original owner");
        let mut snapshot = JournalSnapshot::default();
        snapshot
            .drafts
            .push((owner.clone(), original.draft.clone()));
        assert!(snapshot.queue.admit_operation(QueueOperation::take(operation.clone(), queue_owner, QueueConfiguration::capture(&configuration()),
            QueueTakePurpose::Edit, json(serde_json::json!({"directory":"/project","format":"payload-v1","itemIds":["queued_original"]}))), |_| Ok(())).is_ok());
        let payload = publish(
            journal,
            &mut snapshot,
            &operation,
            "payload",
            document.bytes().to_vec(),
            QueueObjectKind::Payload,
            201,
        );
        let item = QueueTransferItem {
            item: "queued_original".into(),
            index: QueueJson::new(indexed.body().expect("sealed original index"))
                .expect("bounded original index"),
            payload,
            files: Vec::new(),
        };
        let companion = publish(
            journal,
            &mut snapshot,
            &operation,
            "destination",
            serde_json::to_vec(&CompanionDocument::new(original, vec![item.clone()]))
                .expect("lossless original companion"),
            QueueObjectKind::NativeCompanion,
            202,
        );
        let receipt = serde_json::json!({"receipt":{"version":1,"kind":"take","requestId":operation.as_str(),"sessionId":"ses_queue","directory":"/project",
            "requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["queued_original"]},"state":"transferring","revision":1});
        snapshot
            .queue
            .record_take_transfer(
                &operation,
                QueueTransfer {
                    receipt: json(receipt.clone()),
                    items: vec![item],
                },
                &snapshot.attachments,
                |_| Ok(()),
            )
            .expect("real complete transfer");
        snapshot
            .queue
            .prepare_take_destination(
                &operation,
                QueueDestination::Draft { companion },
                json(serde_json::json!({"directory":"/project"})),
                &snapshot.attachments,
                |_| Ok(()),
            )
            .expect("durable original destination");
        save(journal, &snapshot);
        let permit = journal
            .authorize_queue_acknowledgement(&operation, &[files])
            .expect("only actual sealed original data grants acknowledgement authority");
        let mut acknowledged = receipt;
        acknowledged["state"] = serde_json::json!("acknowledged");
        snapshot
            .queue
            .record_take_acknowledged(&permit, json(acknowledged), |_| Ok(()))
            .expect("original transfer is acknowledged once");
        save(journal, &snapshot);
        owner
    }

    fn legacy_path_capture() -> QueueLocalCapture {
        use openchamber_native_transport::queue_manifest::QueuePayloadFileSource;

        let bytes = serde_json::to_vec(&serde_json::json!({"directory":"/project","item":{
            "content":"legacy display","text":"legacy delivery","sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"effort"},
            "attachments":[{"id":"legacy_original","filename":"notes.txt","mimeType":"text/plain","size":999,
                "source":"server","serverPath":"/never/read/server/notes.txt","agentPath":"/original/source/notes.txt","dataUrl":"data:text/plain;base64,b3JpZ2luYWw="}]
        }})).expect("original legacy capture bytes");
        let document =
            QueuePayloadDocument::from_bytes(bytes, "/project", QueuePayloadUse::Enqueue, sha256)
                .expect("typed legacy capture");
        assert!(
            document.files_proof(sha256).is_err(),
            "parsing cannot invent a sealed index or original-file proof"
        );
        let digest = |bytes: &[u8]| {
            sha256(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let hash = digest(document.bytes());
        let length = document.bytes().len();
        let pointer = format!(
            r#"{{"version":1,"encoding":"utf-8-json","totalBytes":{length},"sha256":"{hash}","chunks":[{{"attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":{length},"sha256":"{hash}","blobId":"{}","requestId":"legacy_payload_original","ordinal":0}}]}}"#,
            "ce".repeat(32)
        );
        let index = r#"{"directory":"/project","sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"effort"},"preview":"legacy display","entryCount":1,"contextIds":[],"attachmentBlobIds":[],"promptBytes":512,"admissionBytes":1024,"structuredBytes":0,"legacyAttachmentIds":["legacy_original"]}"#;
        let seal =
            digest(format!(r#"{{"payloadManifest":{pointer},"payloadIndex":{index}}}"#).as_bytes());
        let indexed = QueueIndexedPayload::from_body(format!(r#"{{"format":"payload-v1","payloadManifest":{pointer},"payloadIndex":{index},"payloadIndexHash":"{seal}"}}"#).as_bytes(), "/project", sha256)
            .expect("schema-ordered original legacy index");
        let materialized = indexed
            .materialize(document.bytes().to_vec(), QueuePayloadUse::Enqueue, sha256)
            .expect("actual sealed legacy materialization");
        let proof = materialized
            .files_proof(sha256)
            .expect("only materialized original bytes grant file proof");
        assert_eq!(proof.capture_sha256(), &document.sha256());
        assert_eq!(proof.index_sha256(), &indexed.index_hash());
        assert_eq!(proof.files().len(), 1);
        assert_eq!(
            proof.files()[0].source(),
            &QueuePayloadFileSource::Legacy(0)
        );
        assert_eq!(
            proof.files()[0].byte_len(),
            8,
            "decoded bytes override the advisory legacy size"
        );
        assert_eq!(proof.files()[0].sha256(), &sha256(b"original"));
        let QueuePayloadItem::Legacy(item) = materialized.item() else {
            panic!("original legacy capture");
        };
        assert_eq!(
            item.attachments[0].agent_path.as_deref(),
            Some("/original/source/notes.txt")
        );
        assert_eq!(
            item.attachments[0]
                .bytes()
                .expect("actual inline attachment bytes"),
            b"original"
        );
        let capture = QueueLocalCapture::from_document(materialized, Vec::new(), sha256)
            .expect("explicit conversion of the real legacy file");
        assert_eq!(
            capture.item().files[0].name,
            "/original/source/notes.txt",
            "the local capture preserves the model-facing identity"
        );
        assert!(
            QueuePayloadDocument::from_bytes(
                capture.bytes().to_vec(),
                "/project",
                QueuePayloadUse::Enqueue,
                sha256
            )
            .is_err(),
            "local conversion grants no replacement index or acknowledgement authority"
        );
        capture
    }

    struct RecordedUpload {
        path: String,
        filename: String,
        bytes: Vec<u8>,
    }

    struct RefusedUploadServer {
        endpoint: openchamber_native_transport::endpoint::Endpoint,
        requests: std::sync::Arc<std::sync::Mutex<Vec<RecordedUpload>>>,
        reads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl RefusedUploadServer {
        fn new() -> Self {
            use std::io::{Read, Write};
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};
            use std::time::Duration;

            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("private loopback upload server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking test server");
            let endpoint = openchamber_native_transport::endpoint::Endpoint::parse(&format!(
                "http://{}",
                listener.local_addr().expect("owned address")
            ))
            .expect("captured upload endpoint");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let reads = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let recorded = requests.clone();
            let recorded_reads = reads.clone();
            let stopped = stop.clone();
            let worker = std::thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("owned upload listener failed: {error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .expect("bounded upload read");
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .expect("bounded refusal write");
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream
                            .read_exact(&mut byte)
                            .expect("complete upload headers");
                        headers.push(byte[0]);
                        assert!(headers.len() <= 65536, "bounded fixture headers");
                    }
                    let headers = String::from_utf8(headers).expect("ASCII request headers");
                    let mut first = headers
                        .lines()
                        .next()
                        .expect("request line")
                        .split_whitespace();
                    let method = first.next().expect("request method");
                    let path = first.next().expect("upload path").to_owned();
                    let header = |wanted: &str| {
                        headers
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
                            .map(|(_, value)| value.trim())
                    };
                    if method == "GET" {
                        assert!(
                            header("content-length").is_none_or(|length| length == "0"),
                            "fixture read {path} cannot carry a body"
                        );
                        // Workspace rendering reads routing, permission policy
                        // and the selected directory's Git status independently
                        // of the already seeded composer catalogs.
                        let body = match path.as_str() {
                            "/api/routing" => {
                                r#"{"available":false,"autoReady":false,"jevAvailable":false}"#
                            }
                            "/api/permission-auto-accept" => {
                                r#"{"sessions":{},"modes":{},"revision":1}"#
                            }
                            "/api/git/status?directory=%2Fproject" => {
                                r#"{"isGitRepository":false}"#
                            }
                            _ => panic!("unsupported fixture request {method} {path}"),
                        };
                        recorded_reads
                            .lock()
                            .expect("background read observations")
                            .push(path);
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len())
                            .expect("complete supported background-read response");
                        continue;
                    }
                    assert_eq!(
                        method, "PUT",
                        "this fixture accepts only its named background reads and the real upload boundary: {method} {path}"
                    );
                    let length: usize = header("content-length")
                        .expect("declared byte length")
                        .parse()
                        .expect("valid length");
                    assert!(length <= 1024, "fixture uploads are small");
                    let mut bytes = vec![0; length];
                    stream
                        .read_exact(&mut bytes)
                        .expect("complete raw upload bytes");
                    let filename = header("x-openchamber-filename")
                        .expect("safe upload filename")
                        .to_owned();
                    recorded
                        .lock()
                        .expect("upload observations")
                        .push(RecordedUpload {
                            path,
                            filename,
                            bytes,
                        });
                    // Stop at this boundary. No enqueue receipt, payload index,
                    // acknowledgement, or successful delivery is manufactured.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{{}}"
                    );
                }
            });
            Self {
                endpoint,
                requests,
                reads,
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for RefusedUploadServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !std::thread::panicking() {
                    result.expect("owned upload server stops");
                }
            }
        }
    }

    struct CapturedEditWorkspace {
        window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        owner: DraftKey,
        _fixture: crate::recovery_tests::Fixture,
        server: RefusedUploadServer,
    }

    async fn finish_actual_composer_autosave(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) {
        cx.run_until_parked();
        // Advance the existing autosave debounce on GPUI's test clock. The
        // capture gate itself uses channels and never depends on a timer.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.condition(workspace, |workspace, _| {
            workspace.persistence_ready()
                && !workspace.composer_documents_dirty()
                && !workspace.composer_documents_pending()
        })
        .await;
    }

    async fn open_captured_edit_workspace(cx: &mut TestAppContext) -> CapturedEditWorkspace {
        use openchamber_native_core::send_config::{
            AgentEntry, CatalogModel, CatalogProvider, ModelChoice,
        };
        use openchamber_native_core::session::SessionRecord;
        use openchamber_native_transport::client::ClientDescription;
        use openchamber_native_transport::credential::Credential;

        use crate::connection::{Connection, ServerConfig};
        use crate::send_catalog::{DirectoryCatalog, Load};

        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let (mut journal, _) =
            AttachmentJournal::open(&fixture.journal()).expect("private captured-edit journal");
        let owner = seed_companion(&mut journal, "native");
        drop(journal);
        let server = RefusedUploadServer::new();
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(fixture.journal(), window, cx);
            })
        })
        .expect("hydrate the genuine acknowledged Edit Take");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let mut session = SessionRecord::new(SessionId::from("ses_queue"), "/project");
                session.model = Some(ModelChoice {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    variant: Some("effort".into()),
                });
                session.agent = Some("build".into());
                let load = workspace.sessions.begin_load();
                let _ = workspace.sessions.complete_load(load, vec![session]);
                workspace.selected = Some(SessionId::from("ses_queue"));
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
                        id: "model".into(),
                        provider_model_id: None,
                        name: None,
                        variants: vec!["effort".into()],
                        context_limit: None,
                    }],
                }]);
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory("/project".into(), catalog);
                workspace.send_catalog.settings = Some(Default::default());
                let mut connection = Connection::new(ServerConfig {
                    endpoint: server.endpoint.clone(),
                    password: None,
                    client: ClientDescription::default(),
                })
                .expect("owned captured-edit executor");
                connection.client = Some(
                    ServerClient::new(server.endpoint.clone(), Credential::None)
                        .expect("owned loopback client"),
                );
                connection.state = ConnectionState::Connected;
                connection.loaded = true;
                workspace.connection = Some(connection);
                workspace.composer_key = Some(owner.clone());
                workspace.restore_composer_draft(window, cx);
            })
        })
        .expect("restore the compound editor with a real selected session and catalog");
        cx.condition(&workspace, |workspace, _| {
            workspace.composer_document_ready(&owner)
        })
        .await;
        finish_actual_composer_autosave(&workspace, cx).await;
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("render the connected selected session before editing");
        cx.condition(&workspace, |_, _| {
            server.reads.lock().expect("supported render reads").len() >= 3
        })
        .await;
        let mut reads = server
            .reads
            .lock()
            .expect("actual setup request targets")
            .clone();
        reads.sort();
        assert_eq!(
            reads,
            vec![
                "/api/git/status?directory=%2Fproject",
                "/api/permission-auto-accept",
                "/api/routing"
            ],
            "only the traced render-time reads may precede captured-edit preparation"
        );
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert_eq!(
                    workspace.composer.read(cx).value().as_ref(),
                    "native\n\n#snippet"
                );
                workspace.composer.update(cx, |composer, cx| {
                    composer.focus(window, cx);
                    composer.set_selected_range(6..8, cx);
                });
            });
            window.render_frame(cx);
            workspace.update(cx, |workspace, cx| {
                workspace.composer.update(cx, |composer, cx| {
                    composer.replace_text_in_range(None, " | ", window, cx);
                })
            });
        })
        .expect("one genuine textarea replacement edits only the Source/Literal separator");
        cx.run_until_parked();
        CapturedEditWorkspace {
            window,
            workspace,
            owner,
            _fixture: fixture,
            server,
        }
    }

    #[derive(Clone, Copy)]
    enum CapturedEditInterleaving {
        DurableAutosave,
        DirtyAutosave,
        Clear,
        ClearThenType,
    }

    #[derive(Clone, Copy)]
    enum PendingActor {
        Send,
        Enqueue,
        Prefix,
    }

    #[gpui_kit::test]
    async fn captured_send_pending_edit_after_clear_supports_explicit_local_retry_without_dispatch(
        cx: &mut TestAppContext,
    ) {
        pending_capture_close(PendingActor::Send, true, cx).await;
    }

    #[gpui_kit::test]
    async fn captured_enqueue_pending_edit_after_clear_supports_explicit_local_retry_without_dispatch(
        cx: &mut TestAppContext,
    ) {
        pending_capture_close(PendingActor::Enqueue, true, cx).await;
    }

    #[gpui_kit::test]
    async fn captured_prefix_pending_edit_after_clear_supports_explicit_local_retry_without_dispatch(
        cx: &mut TestAppContext,
    ) {
        pending_capture_close(PendingActor::Prefix, true, cx).await;
    }

    #[gpui_kit::test]
    async fn normal_close_finishes_zero_file_send_capture_after_clear_before_sealing_worker(
        cx: &mut TestAppContext,
    ) {
        pending_capture_close(PendingActor::Send, false, cx).await;
    }

    #[gpui_kit::test]
    async fn normal_close_finishes_zero_file_enqueue_capture_after_clear_before_sealing_worker(
        cx: &mut TestAppContext,
    ) {
        pending_capture_close(PendingActor::Enqueue, false, cx).await;
    }

    #[gpui_kit::test]
    async fn normal_close_finishes_zero_file_prefix_capture_after_clear_before_sealing_worker(
        cx: &mut TestAppContext,
    ) {
        pending_capture_close(PendingActor::Prefix, false, cx).await;
    }

    async fn pending_capture_close(actor: PendingActor, cancel: bool, cx: &mut TestAppContext) {
        use crate::attachment_io::QueueConsumerEditVerifiedCapture;
        use crate::journal_worker::{BlobFailure, ReplyGate};
        use crate::mutation_journal::LoadedJournal;
        use openchamber_native_core::message_queue::{QueueKey, QueuedItemId};
        use std::cell::RefCell;
        use std::rc::Rc;

        type Gate = ReplyGate<Result<QueueConsumerEditVerifiedCapture, BlobFailure>>;
        let CapturedEditWorkspace {
            window,
            workspace,
            owner,
            _fixture,
            server,
        } = open_captured_edit_workspace(cx).await;
        let original_transfer = workspace.read_with(cx, |workspace, _| {
            serde_json::to_vec(
                &workspace
                    .queue_storage
                    .local_take(
                        &QueueOperationId::new("edited_take_original")
                            .expect("genuine Take identity"),
                    )
                    .expect("acknowledged original Take")
                    .transfer,
            )
            .expect("immutable original index, capture and file bindings")
        });
        let reads = server
            .reads
            .lock()
            .expect("initial setup read observations")
            .clone();
        let slot: Rc<RefCell<Option<(QueueOperationId, Gate)>>> = Rc::new(RefCell::new(None));
        let gate_slot = slot.clone();
        let observer = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let observer = cx.observe_self(move |workspace, _| {
                        if gate_slot.borrow().is_some() {
                            return;
                        }
                        let consumer =
                            workspace
                                .queue_storage
                                .operations()
                                .iter()
                                .find(|operation| {
                                    operation.composer_edit().is_some()
                                        && matches!(
                                            (actor, operation.action()),
                                            (PendingActor::Send, QueueAction::Send { .. })
                                                | (
                                                    PendingActor::Enqueue,
                                                    QueueAction::Enqueue { .. }
                                                )
                                                | (
                                                    PendingActor::Prefix,
                                                    QueueAction::Prefix { .. }
                                                )
                                        )
                                });
                        if let Some(consumer) = consumer {
                            let binding = workspace
                                .queue_storage
                                .consumer_edit_binding(consumer.id())
                                .expect("atomic exact pending-edit authority");
                            let verification = workspace
                                .queue_storage
                                .object(&binding.edit().object)
                                .expect("pending edit plan")
                                .verification();
                            let gate = workspace
                                .attachment_worker()
                                .expect("real serialized worker")
                                .hold_consumer_edit_verification_reply_for_test(
                                    binding,
                                    verification,
                                );
                            *gate_slot.borrow_mut() = Some((consumer.id().clone(), gate));
                        }
                    });
                    match actor {
                        PendingActor::Send => assert!(
                            workspace.begin_local_send(None, window, cx),
                            "the real Send entrypoint owns the activation"
                        ),
                        PendingActor::Enqueue => workspace.begin_payload_enqueue(window, cx),
                        PendingActor::Prefix => workspace.begin_payload_prefix(
                            QueueKey::new(
                                owner.runtime().clone(),
                                "/project",
                                SessionId::from("ses_queue"),
                            )
                            .expect("actual captured queue scope"),
                            vec![QueuedItemId::new("queued_next")],
                            None,
                            window,
                            cx,
                        ),
                    }
                    assert!(
                        workspace.composer_submissions_pending(),
                        "window registration precedes the first actor poll"
                    );
                    assert!(
                        !workspace.native_root_actors_pending(),
                        "this capture has zero native files and cannot rely on AttachmentId roots"
                    );
                    workspace
                        .clear_composer_document(&owner, window, cx)
                        .expect("internal Clear only removes current input");
                    assert!(!workspace.composer_documents.contains(&owner));
                    workspace.composer.update(cx, |composer, cx| {
                        composer.replace_text_in_range(None, "new generation input", window, cx);
                        composer.set_selected_range(1..4, cx);
                    });
                    observer
                })
            })
            .expect("start the actual actor and Clear before its first poll");
        cx.condition(&workspace, |_, _| slot.borrow().is_some())
            .await;
        let (id, mut verification) = slot
            .borrow_mut()
            .take()
            .expect("the actual consumer reaches whole-edit verification");
        drop(observer);
        verification.reached().await;
        let current = workspace.read_with(cx, current_editor);
        let mut finish = workspace.update(cx, |workspace, _| {
            let operation = workspace
                .queue_storage
                .operation(&id)
                .expect("typed consumer precedes every edit put");
            let edit = operation
                .composer_edit()
                .expect("one final original edit reference");
            assert!(
                !workspace
                    .queue_storage
                    .object(&edit.object)
                    .expect("retained plan")
                    .verified(),
                "the real proof is computed but not yet accepted"
            );
            assert!(
                workspace.queue_storage.composer_edit(&owner).is_none(),
                "Clear cannot be undone by captured persistence"
            );
            workspace
                .attachment_worker()
                .expect("live exclusive writer")
                .hold_finish_reply_for_test()
        });
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.request_close(window, cx))
        })
        .expect(
            "normal close owns the foreground local-durability barrier even without a Document",
        );
        cx.run_until_parked();
        assert!(
            !finish.has_reached()
                && workspace.read_with(cx, |workspace, _| workspace.composer_submissions_pending())
        );
        assert!(
            matches!(
                AttachmentJournal::open(&_fixture.journal()),
                Err(JournalError::Locked)
            ),
            "held foreground proof retains the window's actual lock"
        );
        if cancel {
            verification.cancel();
        } else {
            verification.release();
        }
        if cancel {
            cx.condition(&workspace, |workspace, _| {
                !workspace.close_in_progress() && !workspace.queue.any_payload_busy()
            })
            .await;
            assert!(!finish.has_reached() && cx.update_window(window, |_, _, _| ()).is_ok());
            workspace.read_with(cx, |workspace, cx| {
                assert!(
                    workspace.composer_submission_failed(&owner)
                        && workspace.composer_submissions_pending()
                );
                assert_eq!(current_editor(workspace, cx), current);
            });
            let (stamp, saved) = workspace.update(cx, |workspace, cx| {
                workspace
                    .persistence_barrier(cx)
                    .expect("cancelled close preserves writer admission")
            });
            let saved = saved.await.expect("same writer remains responsive");
            assert!(workspace.update(cx, |workspace, cx| {
                workspace.record_persistence(stamp, &saved, cx)
            }));
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.retry_composer_document(&owner, window, cx)
                })
            })
            .expect(
                "the existing explicit editor/storage recovery repairs the exact detached edit",
            );
            cx.condition(&workspace, |workspace, _| {
                !workspace.composer_documents_pending()
            })
            .await;
            workspace.read_with(cx, |workspace, cx| {
                assert!(
                    !workspace.composer_submission_failed(&owner)
                        && !workspace.composer_submissions_pending()
                );
                assert_eq!(
                    current_editor(workspace, cx),
                    current,
                    "local recovery preserves exact newer input, selection and undo history"
                );
            });
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| workspace.request_close(window, cx))
            })
            .expect("normal close after explicit local recovery");
        }
        finish.reached().await;
        assert_eq!(
            workspace.read_with(cx, current_editor),
            current,
            "normal close cannot consume or replace a newer generation"
        );
        assert!(
            server
                .requests
                .lock()
                .expect("close dispatch observations")
                .is_empty(),
            "local close/recovery grants no upload, reservation, acknowledgement or prompt request"
        );
        assert_eq!(
            *server.reads.lock().expect("close read observations"),
            reads,
            "close grants no fresh server/source reads"
        );
        let (journal, loaded) = AttachmentJournal::open(&_fixture.journal())
            .expect("worker finish releases the actual profile lock");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("the normal-close capture is durable");
        };
        let operation = restored
            .queue
            .operation(&id)
            .expect("original consuming operation survives restart");
        let edit = operation
            .composer_edit()
            .expect("exact final original edit reference");
        let object = restored
            .queue
            .object(&edit.object)
            .expect("retained verified editing object");
        assert!(object.verified());
        let mut bytes = Vec::new();
        for chunk in object.reference().chunks() {
            bytes.extend_from_slice(
                journal
                    .read_queue(&chunk.descriptor().expect("exact edit descriptor"))
                    .expect("restart-readable editing bytes")
                    .as_slice(),
            );
        }
        let document: serde_json::Value =
            serde_json::from_slice(&bytes).expect("exact encoded editing layout");
        assert_eq!(
            document["text"], "native | #snippet",
            "restart cannot substitute newer input or an unedited Take"
        );
        let Some(QueueSource::Companion(source)) = operation.source() else {
            panic!("normal close publishes the exact original source");
        };
        assert!(
            restored
                .queue
                .object(source)
                .is_some_and(|object| object.verified())
        );
        assert!(
            operation
                .composer_preparation()
                .expect("original preparation marker")
                .sources
                .is_empty(),
            "close never runs mention/snippet or file preparation"
        );
        assert_eq!(
            restored
                .drafts
                .iter()
                .find(|(key, _)| key == &owner)
                .expect("newer current input persists")
                .1
                .text,
            current.text
        );
        assert!(
            restored.queue.draft_companions(&owner).is_empty()
                && restored.queue.composer_edit(&owner).is_none()
        );
        assert_eq!(
            serde_json::to_vec(
                &restored
                    .queue
                    .local_take(&edit.operation)
                    .expect("original Take survives")
                    .transfer
            )
            .expect("unchanged original transfer"),
            original_transfer
        );
        drop(journal);
        finish.release();
        cx.run_until_parked();
        assert!(
            cx.update_window(window, |_, _, _| ()).is_err(),
            "normal close completes only after exact local custody is durable"
        );
    }

    #[gpui_kit::test]
    async fn ordinary_editor_verification_cancel_keeps_input_and_writer_for_explicit_retry_then_normal_close(
        cx: &mut TestAppContext,
    ) {
        use crate::attachment_io::QueueVerifiedCapture;
        use crate::journal_worker::{BlobFailure, ReplyGate};
        use crate::mutation_journal::LoadedJournal;
        use std::cell::RefCell;
        use std::rc::Rc;

        type Gate = ReplyGate<Result<QueueVerifiedCapture, BlobFailure>>;
        let CapturedEditWorkspace {
            window,
            workspace,
            owner,
            _fixture,
            server,
        } = open_captured_edit_workspace(cx).await;
        let slot: Rc<RefCell<Option<Gate>>> = Rc::new(RefCell::new(None));
        let gate_slot = slot.clone();
        let (saving, observer) = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let existing: HashSet<_> = workspace
                        .queue_storage
                        .objects()
                        .iter()
                        .map(|object| object.id().clone())
                        .collect();
                    let observer = cx.observe_self(move |workspace, _| {
                        if gate_slot.borrow().is_some() {
                            return;
                        }
                        if let Some(object) = workspace
                            .queue_storage
                            .objects()
                            .iter()
                            .find(|object| !existing.contains(object.id()))
                        {
                            *gate_slot.borrow_mut() = Some(
                                workspace
                                    .attachment_worker()
                                    .expect("ordinary editor's real worker")
                                    .hold_queue_verification_reply_for_test(object.verification()),
                            );
                        }
                    });
                    let capture = workspace
                        .capture_composer_submission(cx)
                        .expect("capture ordinary dirty input without a consuming operation");
                    assert!(capture.edit.is_none());
                    let saving = cx.spawn_in(window, async move |this, cx| {
                        composer_document::flush(&this, cx, capture).await
                    });
                    (saving, observer)
                })
            })
            .expect("start the real ordinary editor flush");
        cx.condition(&workspace, |_, _| slot.borrow().is_some())
            .await;
        let mut gate = slot
            .borrow_mut()
            .take()
            .expect("actual ordinary editor verification");
        drop(observer);
        gate.reached().await;
        gate.cancel();
        assert!(
            saving.await.is_err(),
            "the real cancelled receipt fails the ordinary flush"
        );
        let current = workspace.read_with(cx, current_editor);
        let mut finish = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    assert!(
                        workspace.composer_document_save_failed(&owner)
                            && workspace.composer_documents_dirty()
                    );
                    let finish = workspace
                        .attachment_worker()
                        .expect("live ordinary writer")
                        .hold_finish_reply_for_test();
                    workspace.request_close(window, cx);
                    assert!(!workspace.close_in_progress() && workspace.persistence_ready());
                    assert!(!workspace.composer_document_ready(&owner));
                    finish
                })
            })
            .expect("normal close refuses the ordinary failed snapshot");
        assert!(!finish.has_reached() && cx.update_window(window, |_, _, _| ()).is_ok());
        assert!(matches!(
            AttachmentJournal::open(&_fixture.journal()),
            Err(JournalError::Locked)
        ));
        let (stamp, saved) = workspace.update(cx, |workspace, cx| {
            workspace
                .persistence_barrier(cx)
                .expect("failed editor retains a usable writer")
        });
        let saved = saved
            .await
            .expect("ordinary failed editor save barrier answers");
        assert!(workspace.update(cx, |workspace, cx| {
            workspace.record_persistence(stamp, &saved, cx)
        }));
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.retry_composer_document(&owner, window, cx)
            })
        })
        .expect("use the existing explicit editor Retry");
        cx.condition(&workspace, |workspace, _| {
            !workspace.composer_documents_pending()
        })
        .await;
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                !workspace.composer_document_save_failed(&owner)
                    && !workspace.composer_documents_dirty()
            );
            assert!(workspace.composer_document_ready(&owner));
            assert_eq!(
                current_editor(workspace, cx),
                current,
                "ordinary Retry preserves exact input, selection and undo history"
            );
            assert!(
                !workspace.composer_submissions_pending(),
                "ordinary editor recovery creates no submission owner"
            );
        });
        assert!(
            server
                .requests
                .lock()
                .expect("ordinary recovery HTTP observations")
                .is_empty()
        );
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.request_close(window, cx))
        })
        .expect("normal close after explicit ordinary recovery");
        finish.reached().await;
        let (journal, loaded) = AttachmentJournal::open(&_fixture.journal())
            .expect("finish releases the ordinary writer's lock");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("ordinary editing state remains durable");
        };
        let edit = restored
            .queue
            .composer_edit(&owner)
            .expect("exact current editing version survives restart");
        assert!(
            restored
                .queue
                .object(&edit.object)
                .is_some_and(|object| object.verified())
        );
        drop(journal);
        finish.release();
        cx.run_until_parked();
        assert!(cx.update_window(window, |_, _, _| ()).is_err());
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CurrentEditor {
        text: String,
        selection: std::ops::Range<usize>,
        checkpoint: u64,
    }

    fn current_editor(workspace: &Workspace, cx: &gpui_kit::App) -> CurrentEditor {
        let composer = workspace.composer.read(cx);
        CurrentEditor {
            text: composer.value().to_string(),
            selection: composer.selected_range(),
            checkpoint: composer.history_checkpoint(),
        }
    }

    #[gpui_kit::test]
    async fn companion_file_reference_chip_removes_only_its_source_occurrence_and_survives_restart(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{ElementId, Role};

        use crate::composer_document::ComposerCompanionInput;
        use crate::queue_storage::QueueTakePhase;

        let CapturedEditWorkspace {
            window,
            workspace,
            owner,
            _fixture,
            server,
        } = open_captured_edit_workspace(cx).await;
        finish_actual_composer_autosave(&workspace, cx).await;
        let (captured, original_take) = workspace.update(cx, |workspace, cx| {
            assert!(workspace.composer_editable() && workspace.composer_document_ready(&owner));
            let captured = workspace
                .capture_composer_submission(cx)
                .expect("capture the hydrated acknowledged Edit Take");
            assert_eq!(captured.composer.references.len(), 1);
            let reference = &captured.composer.references[0];
            assert_eq!(reference.operation.as_str(), "edited_take_original");
            assert_eq!(
                reference.companion.as_str(),
                "edited_take_original-destination"
            );
            let operation = workspace
                .queue_storage
                .operation(&reference.operation)
                .expect("the original Take remains retained");
            assert!(matches!(
                operation.action(),
                QueueAction::Take {
                    purpose: QueueTakePurpose::Edit,
                    phase: QueueTakePhase::Acknowledged { .. },
                    ..
                }
            ));
            let local = workspace
                .queue_storage
                .local_take(&reference.operation)
                .expect("actual acknowledged local destination");
            assert_eq!(local.transfer.items.len(), 1);
            assert_eq!(local.transfer.items[0].item, "queued_original");
            let original_take = serde_json::to_vec(operation)
                .expect("retain the complete original Take and acknowledgement");
            (captured, original_take)
        });
        let mut original_inputs = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        composer_document::materialize(&this, cx, &captured).await
                    })
                })
            })
            .expect("read only the genuine committed companion bytes")
            .await
            .expect("materialize the original source before the pointer action");
        let original_native = original_inputs.native.draft;
        assert_eq!(
            original_inputs.companions.len(),
            1,
            "the separator edit leaves one original rich source"
        );
        let Some(ComposerCompanionInput::Original(original)) = original_inputs.companions.pop()
        else {
            panic!(
                "the companion source has not been edited or filtered before its chip is clicked"
            );
        };
        let original_payload = original.document.bytes().to_vec();
        let original_ids = original.indexed.index().context_ids.clone();
        let original =
            QueueLocalCapture::from_document(original.document, original_ids.clone(), sha256)
                .expect("lossless original source projection");
        assert_eq!(original.item().prompt_parts.file_references.len(), 1);
        assert_eq!(
            original.item().prompt_parts.file_references[0].uri,
            "file:///project/reference.pdf"
        );
        assert_eq!(
            original.item().prompt_parts.context[0].text,
            "original context"
        );
        assert_eq!(original.item().prompt_parts.skills[0].id, "captured-skill");
        let mut expected = original.item().clone();
        expected.prompt_parts.file_references.remove(0);
        let expected = QueueLocalCapture::new("/project", expected, original_ids, sha256)
            .expect("only the exact source's file-reference ordinal is excluded");
        let before_editor = workspace.read_with(cx, current_editor);

        // The stable identity includes the original operation, companion object,
        // item ordinal, reference kind and source-local reference ordinal.
        const CHIP: &str = "composer-reference:edited_take_original:edited_take_original-destination:0:file-reference:0";
        let chip = ElementId::Name(CHIP.into());
        let remove = ElementId::Name(format!("{CHIP}-remove").into());
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let row = window.try_find(chip.clone()).expect(
                "production composer strip is missing the file-reference chip for Take edited_take_original, companion edited_take_original-destination, item 0, file-reference 0",
            );
            assert!(row.visible(), "the captured reference.pdf chip must be visible in the actual composer strip");
            assert!(row.path().contains(&ElementId::Name("composer".into()))
                && row.path().contains(&ElementId::Name("composer-attachments".into())), "the chip belongs to the real composer attachment strip");
            assert!(row.label().is_some_and(|label| label.contains("reference.pdf")), "the chip identifies the captured reference");
            let control = window.try_find(remove.clone()).expect("production companion file-reference chip is missing its per-file removal control");
            assert!(control.visible());
            assert_eq!(control.role(), Some(Role::Button));
            assert_ne!(control.disabled(), Some(true));
            window.click(remove.clone(), cx);
            window.render_frame(cx);
            assert!(window.try_find(chip.clone()).is_none(), "the removed source occurrence leaves the strip");
            assert!(window.try_find(remove.clone()).is_none(), "the removed occurrence leaves no stale control");
        }).expect("activate the actual per-file removal control through native pointer hit testing");
        assert_eq!(
            workspace.read_with(cx, current_editor),
            before_editor,
            "a reference-only removal preserves textarea text, selection and the toolkit undo checkpoint"
        );
        finish_actual_composer_autosave(&workspace, cx).await;
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                current_editor(workspace, cx),
                before_editor,
                "autosave cannot turn reference removal into a text edit"
            );
            let operation =
                QueueOperationId::new("edited_take_original").expect("original Take identity");
            assert_eq!(
                serde_json::to_vec(
                    workspace
                        .queue_storage
                        .operation(&operation)
                        .expect("original Take survives removal")
                )
                .expect("unchanged original Take"),
                original_take,
                "removal preserves original payload/index bindings and acknowledgement inputs"
            );
            assert_eq!(
                workspace.queue_storage.draft_companions(&owner).len(),
                1,
                "independent context and skill still retain their source"
            );
        });
        assert!(
            server
                .requests
                .lock()
                .expect("reference removal dispatch observations")
                .is_empty(),
            "local reference removal performs no upload or queue mutation request"
        );
        let finish = workspace.update(cx, |workspace, _| {
            workspace
                .attachment_worker()
                .expect("real serialized journal owner")
                .finish()
        });
        assert_eq!(
            finish
                .await
                .expect("persist reference-only removal before restart"),
            Ok(CommitOutcome::Durable)
        );

        let (restarted_window, restarted) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("close only the original test window after the fresh workspace exists");
        cx.update_window(restarted_window, |_, window, cx| {
            restarted.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(_fixture.journal(), window, cx);
            })
        })
        .expect("restart over the same real journal without a server connection");
        cx.condition(&restarted, |workspace, _| workspace.persistence_ready())
            .await;
        cx.update_window(restarted_window, |_, window, cx| {
            restarted.update(cx, |workspace, cx| {
                workspace.composer_key = Some(owner.clone());
                workspace.restore_composer_draft(window, cx);
            })
        })
        .expect("restore the persisted compound editor locally");
        cx.condition(&restarted, |workspace, _| {
            workspace.composer_document_ready(&owner)
        })
        .await;
        let restored = cx
            .update_window(restarted_window, |_, window, cx| {
                restarted.update(cx, |workspace, cx| {
                    assert_eq!(
                        workspace.composer.read(cx).value().as_ref(),
                        before_editor.text.as_str()
                    );
                    assert_eq!(
                        workspace.composer.read(cx).selected_range(),
                        before_editor.selection
                    );
                    let operation =
                        QueueOperationId::new("edited_take_original").expect("same original Take");
                    assert_eq!(
                        serde_json::to_vec(
                            workspace
                                .queue_storage
                                .operation(&operation)
                                .expect("original Take survives restart")
                        )
                        .expect("restored original Take"),
                        original_take
                    );
                    let captured = workspace
                        .capture_composer_submission(cx)
                        .expect("capture restored reference-only exclusions");
                    cx.spawn_in(window, async move |this, cx| {
                        let captured = composer_document::flush(&this, cx, captured).await?;
                        let originals =
                            crate::draft_companion::materialize(&this, cx, &captured.composer)
                                .await?;
                        let inputs = composer_document::materialize(&this, cx, &captured).await?;
                        Ok::<_, QueueCaptureFailure>((originals, inputs))
                    })
                })
            })
            .expect("read the restored editing projection and immutable original independently")
            .await
            .expect("restart retains verified local reference removal");
        assert_eq!(restored.0.len(), 1);
        assert_eq!(
            restored.0[0].document.bytes(),
            original_payload.as_slice(),
            "restart still reads the exact original payload bytes"
        );
        assert_eq!(
            restored.1.native.draft, original_native,
            "reference removal preserves the native constituent"
        );
        let [
            ComposerCompanionInput::Edited {
                capture,
                files,
                reprocess_text,
            },
        ] = &restored.1.companions[..]
        else {
            panic!("restart must apply the saved source-local file-reference exclusion");
        };
        assert!(
            !*reprocess_text,
            "removing a reference cannot reprocess the unchanged original expansion"
        );
        assert!(files.is_empty());
        assert_eq!(
            capture.bytes(),
            expected.bytes(),
            "only file-reference ordinal 0 is removed; context, skill, agent, metadata and display/delivery text remain exact"
        );
        let finish = restarted.update(cx, |workspace, _| {
            workspace
                .attachment_worker()
                .expect("restarted journal owner")
                .finish()
        });
        assert_eq!(
            finish.await.expect("drain only the restarted test journal"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(restarted_window, |_, window, _| window.remove_window())
            .expect("close only the restarted test window");
    }

    async fn captured_edit_custody(
        interleaving: CapturedEditInterleaving,
        cx: &mut TestAppContext,
    ) {
        let CapturedEditWorkspace {
            window,
            workspace,
            owner,
            _fixture,
            server,
        } = open_captured_edit_workspace(cx).await;
        if matches!(interleaving, CapturedEditInterleaving::DurableAutosave) {
            finish_actual_composer_autosave(&workspace, cx).await;
        }
        let (id, entered, release) = cx.update_window(window, |_, window, cx| workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.composer.read(cx).value().as_ref(), "native | #snippet");
            assert!(workspace.composer_document_ready(&owner));
            assert!(workspace.send_selection().is_some_and(|(_, selection)| selection.model.is_some()));
            assert!(!workspace.enqueue_blocks_owner(&owner) && !workspace.queue_preview_pending(&SessionId::from("ses_queue")));
            workspace.begin_payload_enqueue(window, cx);
            let actors = workspace.queue.enqueues.tasks.iter().filter(|(_, task)| task.owner.as_ref() == Some(&owner))
                .map(|(id, _)| id.clone()).collect::<Vec<_>>();
            assert_eq!(actors.len(), 1, "the actual enqueue entrypoint must capture and register this submission");
            let id = actors[0].clone();
            assert!(workspace.queue_storage.operation(&id).is_none(), "the captured actor has not admitted its consumer yet");
            let (entered_tx, entered) = tokio::sync::oneshot::channel();
            let (release, release_rx) = tokio::sync::oneshot::channel();
            assert!(workspace.queue.enqueues.capture_gates.insert(id.clone(), EnqueueCaptureGate {
                entered: entered_tx, release: release_rx,
            }).is_none(), "the one-shot gate belongs only to this exact operation in this window");
            if matches!(interleaving, CapturedEditInterleaving::Clear | CapturedEditInterleaving::ClearThenType) {
                workspace.clear_composer_document(&owner, window, cx).expect("internal Clear affects only current input");
                assert!(workspace.composer.read(cx).value().is_empty());
                assert!(!workspace.composer_documents.contains(&owner));
                assert!(workspace.queue_storage.draft_companions(&owner).is_empty());
                assert!(workspace.queue_storage.composer_edit(&owner).is_none());
                assert!(workspace.queue.enqueues.busy(&id), "Clear must leave the already captured submission running");
                if matches!(interleaving, CapturedEditInterleaving::ClearThenType) {
                    workspace.composer.update(cx, |composer, cx| {
                        composer.replace_text_in_range(None, "new generation input", window, cx);
                        composer.set_selected_range(1..4, cx);
                    });
                }
            }
            (id, entered, release)
        })).expect("capture, arm the exact actor gate and optionally Clear before the first executor yield");
        let captured = entered
            .await
            .expect("the actual actor reaches its pre-flush capture gate");
        assert_eq!(
            captured.composer.native.draft.text, "native",
            "the capture retains the original native constituent"
        );
        assert_eq!(
            captured.composer.references.len(),
            1,
            "only the genuine original Edit Take supplies the companion"
        );
        if matches!(interleaving, CapturedEditInterleaving::DurableAutosave) {
            assert!(
                captured.edit.is_some(),
                "E1 must already have verified ordinary-autosave custody at submit"
            );
        } else {
            assert!(
                captured.edit.is_none(),
                "this case must pause the dirty submitted snapshot before encoding"
            );
        }
        assert!(
            server
                .requests
                .lock()
                .expect("captured upload observations")
                .is_empty(),
            "the gate precedes every outgoing request"
        );
        let original_transfer = workspace.read_with(cx, |workspace, _| {
            serde_json::to_vec(
                &workspace
                    .queue_storage
                    .local_take(&captured.composer.references[0].operation)
                    .expect("genuine original Take remains")
                    .transfer,
            )
            .expect("immutable original transfer")
        });
        if matches!(
            interleaving,
            CapturedEditInterleaving::DurableAutosave | CapturedEditInterleaving::DirtyAutosave
        ) {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| workspace.composer.update(cx, |composer, cx| {
                    let end = composer.value().len();
                    composer.set_selected_range(end..end, cx);
                }));
                window.input(" newer", cx);
                workspace.update(cx, |workspace, cx| workspace.composer.update(cx, |composer, cx| composer.set_selected_range(1..4, cx)));
            }).expect("ordinary typing and selection advance the live compound document while its earlier actor is held");
            finish_actual_composer_autosave(&workspace, cx).await;
            workspace.read_with(cx, |workspace, _| {
                let newer = workspace
                    .queue_storage
                    .composer_edit(&owner)
                    .expect("E2 is the actual current autosave");
                assert!(
                    newer.revision > captured.revision,
                    "the newer autosave must overtake the submitted revision before admission"
                );
                assert!(
                    workspace
                        .queue_storage
                        .object(&newer.object)
                        .is_some_and(|object| object.verified())
                );
                assert!(
                    workspace.queue_storage.operation(&id).is_none(),
                    "E2 publication and retirement run before consumer admission"
                );
            });
        }
        let expected_current = workspace.read_with(cx, current_editor);
        release
            .send(())
            .expect("release only the captured actor after the required interleaving");
        cx.condition(&workspace, |workspace, _| {
            !workspace.queue.enqueues.busy(&id)
        })
        .await;
        let (retained, retained_edit_verified, actual_current, current_edit, original_after) =
            workspace.read_with(cx, |workspace, cx| {
                let retained = workspace.queue_storage.operation(&id).cloned();
                let verified = retained
                    .as_ref()
                    .and_then(QueueOperation::composer_edit)
                    .and_then(|edit| workspace.queue_storage.object(&edit.object))
                    .is_some_and(|object| object.verified());
                (
                    retained,
                    verified,
                    current_editor(workspace, cx),
                    workspace.queue_storage.composer_edit(&owner).cloned(),
                    serde_json::to_vec(
                        &workspace
                            .queue_storage
                            .local_take(&captured.composer.references[0].operation)
                            .expect("original Take is retained independently")
                            .transfer,
                    )
                    .expect("original transfer after the actor"),
                )
            });
        let source = if retained.is_some() {
            let operation = id.clone();
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        compound_source(&this, cx, &operation).await
                    })
                })
            })
            .expect("read the actual consumer's committed source")
            .await
        } else {
            Err(QueueCaptureFailure::Unavailable)
        };
        let uploads = server
            .requests
            .lock()
            .expect("actual captured payload uploads")
            .iter()
            .map(|request| {
                (
                    request.path.clone(),
                    request.filename.clone(),
                    request.bytes.clone(),
                )
            })
            .collect::<Vec<_>>();
        let finish = workspace.update(cx, |workspace, _| {
            workspace
                .attachment_worker()
                .expect("real serialized byte owner")
                .finish()
        });
        assert_eq!(
            finish.await.expect("captured-edit journal drain"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("close only this test's window before reporting custody assertions");

        assert!(
            retained.is_some(),
            "the captured compound edit lost consumer custody before admission after current input advanced or was cleared"
        );
        let retained = retained.expect("the original captured operation must remain recoverable");
        let source =
            source.expect("the original typed source must reach verified companion custody");
        let edit = retained
            .composer_edit()
            .expect("the consumer must retain its exact submitted editing snapshot");
        assert_eq!(
            (edit.generation, edit.revision),
            (captured.generation, captured.revision)
        );
        if let Some(original) = &captured.edit {
            assert!(
                edit == original,
                "an already verified E1 cannot be replaced with E2"
            );
        }
        assert!(
            retained_edit_verified,
            "the retained consumer must own verified exact edit bytes before its payload upload"
        );
        assert!(source.intent.edit.as_ref() == Some(edit));
        assert!(source.composer.references == captured.composer.references);
        assert_eq!(source.composer.native.draft, captured.composer.native.draft);
        assert_eq!(retained.configuration().restore(), configuration());
        assert!(
            matches!(
                retained.action(),
                QueueAction::Enqueue {
                    phase: QueueEnqueuePhase::Capturing,
                    source: QueueSource::Companion(_),
                    ..
                }
            ),
            "the scripted refusal stops at upload without inventing enqueue admission"
        );
        assert_eq!(
            original_after, original_transfer,
            "capture custody cannot rewrite the original Take, index or acknowledgement inputs"
        );
        assert_eq!(
            actual_current, expected_current,
            "the captured actor must preserve current input, selection and toolkit history"
        );
        if matches!(
            interleaving,
            CapturedEditInterleaving::Clear | CapturedEditInterleaving::ClearThenType
        ) {
            assert!(
                current_edit.is_none(),
                "the old submission must not restore its editing snapshot as current after Clear"
            );
        }
        assert_eq!(
            uploads.len(),
            1,
            "the actual actor must carry its captured edit through the first real payload PUT"
        );
        assert_eq!(
            uploads[0].0.split('?').next(),
            Some(
                format!(
                    "/api/message-queue/sessions/ses_queue/requests/{}/attachments/payload-0",
                    id.as_str()
                )
                .as_str()
            )
        );
        assert_eq!(uploads[0].1, "payload-0.json");
        let payload = QueuePayloadDocument::from_bytes(
            uploads[0].2.clone(),
            "/project",
            QueuePayloadUse::Enqueue,
            sha256,
        )
        .expect("the actual uploaded bytes form a complete typed payload");
        let QueuePayloadItem::Manifest(manifest) = payload.item() else {
            panic!("the real composer publishes a manifest capture");
        };
        assert_eq!(
            manifest.content, "native | #snippet",
            "the uploaded display follows the captured Source/Literal layout"
        );
        assert_eq!(
            manifest.text, "native | original expansion",
            "delivery preserves the original companion expansion and excludes newer input"
        );
        assert!(
            manifest.attachment_manifest.is_empty(),
            "reference-only files cannot become invented byte uploads"
        );
        assert_eq!(
            manifest.prompt_parts.file_references[0].uri,
            "file:///project/reference.pdf"
        );
        assert_eq!(manifest.prompt_parts.skills[0].id, "captured-skill");
        assert_eq!(manifest.prompt_parts.context[0].text, "original context");
    }

    #[gpui_kit::test]
    async fn captured_verified_compound_edit_survives_newer_autosave_before_enqueue_admission(
        cx: &mut TestAppContext,
    ) {
        captured_edit_custody(CapturedEditInterleaving::DurableAutosave, cx).await;
    }

    #[gpui_kit::test]
    async fn captured_dirty_compound_edit_survives_newer_autosave_before_encoding(
        cx: &mut TestAppContext,
    ) {
        captured_edit_custody(CapturedEditInterleaving::DirtyAutosave, cx).await;
    }

    #[gpui_kit::test]
    async fn captured_compound_enqueue_survives_internal_clear_before_its_first_poll(
        cx: &mut TestAppContext,
    ) {
        captured_edit_custody(CapturedEditInterleaving::Clear, cx).await;
    }

    #[gpui_kit::test]
    async fn captured_compound_enqueue_preserves_new_generation_typing_after_internal_clear(
        cx: &mut TestAppContext,
    ) {
        captured_edit_custody(CapturedEditInterleaving::ClearThenType, cx).await;
    }

    #[gpui_kit::test]
    async fn compound_upload_uses_a_safe_filename_for_a_materialized_legacy_agent_path(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_transport::credential::Credential;

        use crate::attachment_catalog::{PreparationHold, PreparedAttachment};

        cx.executor().allow_parking();
        let capture = legacy_path_capture();
        let before = capture.bytes().to_vec();
        let fixture = crate::recovery_tests::Fixture::new();
        let (mut journal, _) =
            AttachmentJournal::open(&fixture.journal()).expect("private byte owner");
        let mut native = native();
        native.draft = Draft::new("legacy display");
        let owner = native.owner.clone();
        let blob =
            BlobId::parse("00000000000000000000000000000301").expect("private prepared-file label");
        let hold = PreparationHold {
            owner: owner.clone(),
            id: AttachmentId::from(blob.as_str()),
            generation: 1,
            blob,
        };
        let mut snapshot = JournalSnapshot::default();
        snapshot.drafts.push((owner.clone(), native.draft.clone()));
        snapshot.preparations.push(hold.clone());
        save(&mut journal, &snapshot);
        let receipt = journal
            .put(&hold, b"original")
            .expect("actual owned prepared bytes");
        let descriptor = receipt.descriptor;
        snapshot.preparations.clear();
        snapshot.attachments.insert(
            hold.id.clone(),
            PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        native.draft.content.files.push(DraftFile {
            id: hold.id,
            name: Some(capture.item().files[0].name.clone()),
            media_type: Some("text/plain".into()),
            byte_len: Some(8),
            source_document_id: None,
            content: DraftFileContent::Prepared,
        });
        snapshot.drafts[0].1 = native.draft.clone();
        let id = QueueOperationId::new("legacy_path_enqueue").expect("captured upload request");
        let queue_owner = QueueOwner::new(
            owner.runtime(),
            "/project",
            &SessionId::from("ses_queue"),
            &owner,
        )
        .expect("exact original owner");
        let operation = QueueOperation::enqueue_with_edit(
            id.clone(),
            queue_owner,
            QueueConfiguration::capture(&configuration()),
            CompanionCapture {
                native,
                references: Vec::new(),
            },
            Vec::new(),
            vec![QueueNativeFileDescriptor {
                id: blob.as_str().into(),
                byte_len: descriptor.byte_len(),
                sha256: *descriptor.sha256(),
            }],
            None,
        )
        .expect("retained original source and byte roots");
        let source = operation
            .enqueue_capture()
            .expect("immutable source metadata");
        let source_sha256 = *operation.source_sha256().expect("original source seal");
        assert!(
            snapshot
                .queue
                .admit_operation(operation, |_| Ok(()))
                .is_ok()
        );
        save(&mut journal, &snapshot);
        drop(journal);
        let prepared_capture =
            QueueLocalCapture::from_bytes(capture.bytes().to_vec(), "/project", sha256)
                .expect("exact independent prepared capture");
        let input =
            PreparedComposerInput::new(prepared_capture, vec![descriptor], 1, 1, source_sha256)
                .expect("actual prepared input bindings");
        let restored = PreparedComposerInput::decode(
            &input.encode().expect("retained prepared metadata"),
            "/project",
        )
        .expect("prepared input round trip");
        assert_eq!(restored.capture.bytes(), before);
        assert_eq!(
            restored.capture.item().files[0].name,
            "/original/source/notes.txt"
        );
        let server = RefusedUploadServer::new();
        let network = crate::network::Network::new().expect("owned upload executor");
        let client = ServerClient::new(server.endpoint.clone(), Credential::None)
            .expect("loopback upload client");
        let scope = QueueScope::new(&client, SessionId::from("ses_queue"), "/project")
            .expect("captured exact directory");
        let captured_server = CapturedServer {
            runtime: owner.runtime().clone(),
            client,
            executor: network.handle(),
            owner: scope
                .request(id.as_str())
                .expect("immutable upload label owner"),
        };
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(fixture.journal(), window, cx);
            })
        })
        .expect("hydrate the real byte catalog and source");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        let captured_id = id.clone();
        let result = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.queue.enqueues.tasks.insert(
                        id.clone(),
                        EnqueueTask {
                            runtime: owner.runtime().clone(),
                            owner: Some(owner.clone()),
                            native_roots: Vec::new(),
                            _task: Task::ready(()),
                        },
                    );
                    cx.spawn_in(window, async move |this, cx| {
                        finish_compound_preparation(
                            &this,
                            cx,
                            &captured_id,
                            &captured_server,
                            &source,
                            input,
                            None,
                            Activation::Initial,
                        )
                        .await
                    })
                })
            })
            .expect("run the actual compound upload callsite")
            .await;
        assert!(
            result.is_err(),
            "the scripted server deliberately refuses after recording the first upload"
        );
        {
            let requests = server.requests.lock().expect("actual upload observations");
            assert_eq!(
                requests.len(),
                1,
                "a materialized model-facing path must reach QueueUpload through a safe filename"
            );
            assert_eq!(
                requests[0].path.split('?').next(),
                Some(
                    "/api/message-queue/sessions/ses_queue/requests/legacy_path_enqueue/attachments/file-0"
                )
            );
            assert_eq!(requests[0].filename, "notes.txt");
            assert_eq!(
                requests[0].bytes, b"original",
                "the upload reads verified local bytes without reopening either captured path"
            );
        }
        assert_eq!(
            capture.bytes(),
            before,
            "upload naming cannot rewrite the original model-facing local capture"
        );
        workspace.update(cx, |workspace, _| {
            workspace.queue.enqueues.tasks.remove(&id);
            let source = workspace
                .queue_storage
                .operation(&id)
                .expect("original unresolved owner")
                .enqueue_capture()
                .expect("original source metadata");
            assert_eq!(
                source.composer.native.draft.content.files[0]
                    .name
                    .as_deref(),
                Some("/original/source/notes.txt")
            );
        });
        let finish = workspace.update(cx, |workspace, _| {
            workspace.attachment_worker().expect("real worker").finish()
        });
        assert_eq!(
            finish.await.expect("worker drain"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("close the test-owned window");
    }

    #[gpui_kit::test]
    async fn begin_payload_enqueue_starts_the_captured_actor_for_a_companion_only_composer(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_core::send_config::{CatalogModel, CatalogProvider, ModelChoice};
        use openchamber_native_core::session::SessionRecord;
        use openchamber_native_transport::client::ClientDescription;
        use openchamber_native_transport::credential::Credential;
        use openchamber_native_transport::endpoint::Endpoint;

        use crate::connection::{Connection, ServerConfig};
        use crate::send_catalog::{DirectoryCatalog, Load};

        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let (mut journal, _) =
            AttachmentJournal::open(&fixture.journal()).expect("private journal");
        let owner = seed_companion(&mut journal, "");
        drop(journal);
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(fixture.journal(), window, cx);
            })
        })
        .expect("real local storage owner");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let mut session = SessionRecord::new(SessionId::from("ses_queue"), "/project");
                session.model = Some(ModelChoice {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    variant: Some("effort".into()),
                });
                let load = workspace.sessions.begin_load();
                let _ = workspace.sessions.complete_load(load, vec![session]);
                workspace.selected = Some(SessionId::from("ses_queue"));
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(Vec::new());
                catalog.providers = Load::Ready(vec![CatalogProvider {
                    id: "provider".into(),
                    name: None,
                    models: vec![CatalogModel {
                        id: "model".into(),
                        provider_model_id: None,
                        name: None,
                        variants: vec!["effort".into()],
                        context_limit: None,
                    }],
                }]);
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory("/project".into(), catalog);
                workspace.send_catalog.settings = Some(Default::default());
                workspace.composer_key = Some(owner.clone());
                workspace.restore_composer_draft(window, cx);
            })
        })
        .expect("restore the actual companion-only composer under its selected session");
        cx.condition(&workspace, |workspace, _| {
            workspace.composer_document_ready(&owner)
        })
        .await;
        cx.update_window(window, |_, window, cx| workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.composer.read(cx).value().as_ref(), "#snippet");
            let captured = workspace.capture_composer_submission(cx).expect("complete original compound source");
            assert!(!captured.composer.native.draft.has_sendable_content(), "the native constituent is genuinely empty");
            assert_eq!(captured.composer.references.len(), 1);
            assert_eq!(workspace.compound_composer_sendable(), Some(true));
            assert!(workspace.send_selection().is_some_and(|(_, selection)| selection.model.is_some()), "model readiness cannot explain an early return");
            assert!(!workspace.enqueue_blocks_owner(&owner));
            assert!(!workspace.queue_preview_pending(&SessionId::from("ses_queue")));
            assert!(!workspace.attachment_storage.has_pending(&owner));
            assert!(!workspace.attachment_preparations.iter().any(|hold| hold.owner == owner));
            let endpoint = Endpoint::parse("https://example.invalid/").expect("inert endpoint");
            let mut connection = Connection::new(ServerConfig { endpoint: endpoint.clone(), password: None, client: ClientDescription::default() })
                .expect("owned network runtime");
            connection.client = Some(ServerClient::new(endpoint, Credential::None).expect("captured client"));
            connection.state = ConnectionState::Connected;
            workspace.connection = Some(connection);
            workspace.begin_payload_enqueue(window, cx);
            let actors = workspace.queue.enqueues.tasks.iter().filter(|(_, task)| task.owner.as_ref() == Some(&owner))
                .map(|(id, _)| id.clone()).collect::<Vec<_>>();
            // Observe and cancel synchronously, before the spawned actor can do
            // any disk or network work. A failure notification grants no actor.
            for id in &actors { workspace.queue.enqueues.tasks.remove(id); }
            assert_eq!(actors.len(), 1, "the real enqueue entrypoint must acquire an actor for the complete sendable composer");
            assert_eq!(workspace.composer.read(cx).value().as_ref(), "#snippet", "preflight never consumes the original source");
            assert_eq!(workspace.queue_storage.draft_companions(&owner), captured.composer.references);
        })).expect("a sendable companion survives native-only preflight");
        let finish = workspace.update(cx, |workspace, _| {
            workspace.attachment_worker().expect("real worker").finish()
        });
        assert_eq!(
            finish.await.expect("worker drain"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("close the test-owned window");
    }

    #[gpui_kit::test]
    async fn failed_pre_admission_enqueue_retires_removed_native_bytes_after_its_actual_actor_releases(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_core::send_config::{CatalogModel, CatalogProvider, ModelChoice};
        use openchamber_native_core::session::SessionRecord;
        use openchamber_native_transport::client::ClientDescription;
        use openchamber_native_transport::credential::Credential;
        use openchamber_native_transport::endpoint::Endpoint;

        use crate::attachment_catalog::{PreparationHold, PreparedAttachment};
        use crate::connection::{Connection, ServerConfig};
        use crate::mutation_journal::LoadedJournal;
        use crate::send_catalog::{DirectoryCatalog, Load};

        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let (mut journal, _) =
            AttachmentJournal::open(&fixture.journal()).expect("private original journal");
        let owner = seed_companion(&mut journal, "native");
        drop(journal);
        let (mut journal, loaded) =
            AttachmentJournal::open(&fixture.journal()).expect("load the genuine compound source");
        let LoadedJournal::Present(mut snapshot) = loaded else {
            panic!("the original companion is durable");
        };
        let removed_blob =
            BlobId::parse("00000000000000000000000000000401").expect("removed native label");
        let shared_blob = BlobId::parse("00000000000000000000000000000402")
            .expect("independently owned sibling label");
        let removed_id = AttachmentId::from(removed_blob.as_str());
        let shared_id = AttachmentId::from(shared_blob.as_str());
        let group = AttachmentId::from("pre_admission_source_group");
        let bytes: [&[u8]; 2] = [
            b"removed native bytes",
            b"independently owned sibling bytes",
        ];
        let holds = [removed_blob, shared_blob].map(|blob| PreparationHold {
            owner: owner.clone(),
            id: AttachmentId::from(blob.as_str()),
            generation: 1,
            blob,
        });
        snapshot.preparations.extend_from_slice(&holds);
        save(&mut journal, &snapshot);
        let mut files = Vec::new();
        let members = holds.iter().zip(bytes).zip(["removed.txt", "shared.txt"]);
        for ((hold, bytes), filename) in members {
            let receipt = journal
                .put(hold, bytes)
                .expect("real prepared bytes under their committed hold");
            snapshot.attachments.insert(
                hold.id.clone(),
                PreparedAttachment {
                    blob: receipt.descriptor,
                    staging: None,
                },
            );
            files.push(DraftFile {
                id: hold.id.clone(),
                name: Some(filename.into()),
                media_type: Some("text/plain".into()),
                byte_len: Some(receipt.descriptor.byte_len()),
                source_document_id: Some(group.clone()),
                content: DraftFileContent::Prepared,
            });
        }
        snapshot.preparations.clear();
        snapshot
            .drafts
            .iter_mut()
            .find(|(key, _)| key == &owner)
            .expect("current native constituent")
            .1
            .content
            .files = files.clone();
        let independent_owner = DraftKey::new(
            owner.runtime().clone(),
            "/project",
            DraftTarget::Session(SessionId::from("ses_independent")),
        )
        .expect("independent exact draft owner");
        let mut independent = Draft::new("independent attachment owner");
        independent.content.files.push(files[1].clone());
        snapshot
            .drafts
            .push((independent_owner.clone(), independent.clone()));
        let shared_descriptor = snapshot.attachments[&shared_id].blob;
        save(&mut journal, &snapshot);
        drop(journal);
        let blob_path = |blob: &BlobId| {
            fixture
                .journal()
                .parent()
                .expect("private profile")
                .join("attachment-spool")
                .join(format!("blob-{}.bin", blob.as_str()))
        };
        let removed_path = blob_path(&removed_blob);
        let shared_path = blob_path(&shared_blob);
        assert_eq!(
            std::fs::read(&removed_path).expect("original removed-file bytes"),
            bytes[0]
        );
        assert_eq!(
            std::fs::read(&shared_path).expect("original sibling bytes"),
            bytes[1]
        );

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(fixture.journal(), window, cx);
            })
        })
        .expect("hydrate the original source and real byte catalog");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let mut session = SessionRecord::new(SessionId::from("ses_queue"), "/project");
                session.model = Some(ModelChoice {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    variant: Some("effort".into()),
                });
                let load = workspace.sessions.begin_load();
                let _ = workspace.sessions.complete_load(load, vec![session]);
                workspace.selected = Some(SessionId::from("ses_queue"));
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(Vec::new());
                catalog.providers = Load::Ready(vec![CatalogProvider {
                    id: "provider".into(),
                    name: None,
                    models: vec![CatalogModel {
                        id: "model".into(),
                        provider_model_id: None,
                        name: None,
                        variants: vec!["effort".into()],
                        context_limit: None,
                    }],
                }]);
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory("/project".into(), catalog);
                workspace.send_catalog.settings = Some(Default::default());
                workspace.composer_key = Some(owner.clone());
                workspace.restore_composer_draft(window, cx);
            })
        })
        .expect("restore the real compound editor under its selected session");
        cx.condition(&workspace, |workspace, _| {
            workspace.composer_document_ready(&owner)
        })
        .await;
        let refusal_owner = DraftKey::new(
            owner.runtime().clone(),
            "/project",
            DraftTarget::Session(SessionId::from("ses_refusal_owner")),
        )
        .expect("different exact refusal owner");
        let (id, refusal_bytes, captured_revision) = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    assert_eq!(
                        workspace.composer.read(cx).value().as_ref(),
                        "native\n\n#snippet"
                    );
                    // A selection change guarantees a dirty compound snapshot even if
                    // the restoration's ordinary autosave has already completed.
                    let selection = if workspace.composer.read(cx).selected_range() == (0..1) {
                        0..0
                    } else {
                        0..1
                    };
                    workspace.composer.update(cx, |composer, cx| {
                        composer.set_selected_range(selection, cx)
                    });
                    let capture = workspace
                        .capture_composer_submission(cx)
                        .expect("capture the real dirty compound document");
                    assert!(
                        capture.edit.is_none() && workspace.composer_documents_dirty(),
                        "the actor must flush this exact snapshot before admission"
                    );
                    assert_eq!(capture.composer.references.len(), 1);
                    assert_eq!(capture.composer.native.draft.content.files, files);
                    let endpoint = Endpoint::parse("https://example.invalid/")
                        .expect("inert captured endpoint");
                    let mut connection = Connection::new(ServerConfig {
                        endpoint: endpoint.clone(),
                        password: None,
                        client: ClientDescription::default(),
                    })
                    .expect("owned executor without connection work");
                    connection.client = Some(
                        ServerClient::new(endpoint, Credential::None).expect("captured client"),
                    );
                    connection.state = ConnectionState::Connected;
                    workspace.connection = Some(connection);
                    workspace.begin_payload_enqueue(window, cx);
                    let actors = workspace
                        .queue
                        .enqueues
                        .tasks
                        .iter()
                        .filter(|(_, task)| task.owner.as_ref() == Some(&owner))
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        actors.len(),
                        1,
                        "the real entrypoint starts the pre-admission actor"
                    );
                    let id = actors[0].clone();
                    assert!(workspace.queue_storage.operation(&id).is_none());
                    assert!(
                        workspace.queue.enqueues.references_native(&removed_id)
                            && workspace.queue.enqueues.references_native(&shared_id)
                            && workspace.queue.enqueues.references_native(&group),
                        "the captured file and source-group roots precede the first async flush"
                    );

                    // These operations share one foreground update. The spawned actor
                    // cannot run between capture, user removal and the bounded refusal.
                    workspace.remove_composer_attachment(&owner, &removed_id, window, cx);
                    assert!(
                        workspace
                            .drafts
                            .get(&owner)
                            .expect("current native draft")
                            .content
                            .files
                            .is_empty(),
                        "user removal clears the whole source group"
                    );
                    assert!(
                        !workspace.queue_storage.references_native(&removed_id),
                        "the enqueue has no ledger-owned file root yet"
                    );
                    assert!(
                        workspace.attachment_has_owner(&removed_id),
                        "only the actual actor still protects the removed file"
                    );
                    assert!(workspace.attachments.contains_key(&removed_id));
                    assert!(
                        !workspace.retired_attachment_blobs.contains(&removed_blob)
                            && !workspace.attachment_retirement.busy(&removed_blob),
                        "removal defers retirement while the actor retains its source"
                    );
                    assert!(
                        removed_path.is_file() && shared_path.is_file(),
                        "no captured bytes can be unlinked before root release"
                    );
                    assert_eq!(workspace.drafts.get(&independent_owner), Some(&independent));

                    // This is a valid, unrelated operation, not a malformed journal or
                    // a close gate. Its different owner makes compound_allowed refuse
                    // after the original actor's real flush, before any upload or POST.
                    let queue_owner = QueueOwner::new(
                        refusal_owner.runtime(),
                        "/project",
                        &SessionId::from("ses_refusal_owner"),
                        &refusal_owner,
                    )
                    .expect("exact independent refusal owner");
                    let unrelated = QueueOperation::enqueue(
                        id.clone(),
                        queue_owner,
                        QueueConfiguration::capture(&configuration()),
                        QueueNativeSource {
                            owner: refusal_owner.clone(),
                            draft: Draft::new("unrelated admitted original"),
                        },
                    )
                    .expect("valid independent source without file roots");
                    let refusal_bytes =
                        serde_json::to_vec(&unrelated).expect("immutable unrelated operation");
                    workspace
                        .change_queue_storage(|queue, workspace| {
                            queue
                                .admit_operation(unrelated, |candidate| {
                                    workspace.validate_queue_candidate(candidate)
                                })
                                .map_err(|(error, _)| error)
                        })
                        .expect("bounded canonical admission refusal injection");
                    assert!(
                        workspace.persistence_ready() && !workspace.close_in_progress(),
                        "storage and retirement remain usable"
                    );
                    (id, refusal_bytes, capture.revision)
                })
            })
            .expect("capture, remove and inject refusal before the actor's first executor yield");

        cx.condition(&workspace, |workspace, _| {
            !workspace.queue.enqueues.busy(&id)
        })
        .await;
        cx.run_until_parked();
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.queue_storage.composer_edit(&owner).is_none_or(|edit| edit.revision < captured_revision),
                "refused atomic admission cannot publish its pending edit as current input");
            assert!(!workspace.queue.enqueues.references_native(&removed_id));
            assert!(workspace.attachment_has_owner(&removed_id) && workspace.queue_storage.references_captured_native(&removed_id),
                "task-independent registered custody survives an admission refusal after user removal");
            assert!(workspace.composer_submissions_pending() && workspace.composer_submission_failed(&owner));
            assert_eq!(serde_json::to_vec(workspace.queue_storage.operation(&id).expect("unrelated owner survives")).expect("unchanged independent operation"), refusal_bytes);
            assert_eq!(workspace.queue_storage.operation(&id).expect("exact independent operation").owner().draft_key().expect("valid owner"), refusal_owner);
            assert!(workspace.drafts.get(&owner).expect("original composer owner").content.files.is_empty(), "failure cannot restore files the user removed");
            assert_eq!(workspace.composer.read(cx).value().as_ref(), "native\n\n#snippet");
            assert_eq!(workspace.drafts.get(&independent_owner), Some(&independent));
            assert!(workspace.attachments.contains_key(&shared_id) && workspace.attachment_has_owner(&shared_id));
            assert!(!workspace.retired_attachment_blobs.contains(&shared_blob), "the independently owned sibling never enters retirement");
            assert!(workspace.persistence_ready() && !workspace.close_in_progress());
        });
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.request_close(window, cx))
        })
        .expect(
            "normal close attempts local admission without replacing the conflicting operation",
        );
        cx.condition(&workspace, |workspace, _| !workspace.close_in_progress())
            .await;
        assert!(workspace.read_with(cx, |workspace, _| workspace.composer_submissions_pending()));
        assert!(
            cx.update_window(window, |_, _, _| ()).is_ok(),
            "normal close retains the refused capture, window and writer"
        );
        cx.condition(&workspace, |workspace, _| {
            !workspace.attachment_retirement.busy(&removed_blob)
        })
        .await;
        let (stamp, saved) = workspace.update(cx, |workspace, cx| {
            workspace
                .persistence_barrier(cx)
                .expect("valid post-completion snapshot")
        });
        let saved = saved.await.expect("post-completion save answers");
        assert!(
            workspace.update(cx, |workspace, cx| workspace
                .record_persistence(stamp, &saved, cx)),
            "post-completion retirement is durably representable"
        );
        let finish = workspace.update(cx, |workspace, _| {
            workspace
                .attachment_worker()
                .expect("real serialized byte owner")
                .finish()
        });
        assert_eq!(
            finish
                .await
                .expect("test-only forced teardown drains accepted journal work"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("forced test teardown is outside normal-quit custody guarantees");

        let (journal, loaded) = AttachmentJournal::open(&fixture.journal())
            .expect("restart reads the final authoritative state");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("post-completion snapshot is durable");
        };
        assert!(
            restored.attachments.contains_key(&removed_id) && removed_path.is_file(),
            "a refused capture cannot authorize byte release merely because its actor ended"
        );
        assert_eq!(
            restored.attachments.get(&shared_id).map(|entry| entry.blob),
            Some(shared_descriptor)
        );
        assert_eq!(
            journal
                .read(&shared_descriptor)
                .expect("independent sibling remains readable")
                .as_slice(),
            bytes[1]
        );
        assert!(shared_path.is_file());
        assert!(
            restored
                .drafts
                .iter()
                .find(|(key, _)| key == &owner)
                .expect("original owner survives restart")
                .1
                .content
                .files
                .is_empty()
        );
        assert_eq!(
            restored
                .drafts
                .iter()
                .find(|(key, _)| key == &independent_owner)
                .map(|(_, draft)| draft),
            Some(&independent)
        );
        assert_eq!(
            serde_json::to_vec(
                restored
                    .queue
                    .operation(&id)
                    .expect("independent refusal owner persists")
            )
            .expect("same unrelated operation after restart"),
            refusal_bytes
        );
    }

    #[derive(Clone, Copy)]
    enum VerificationCompletion {
        Release,
        Cancel,
    }

    #[gpui_kit::test]
    async fn normal_close_cannot_finish_before_an_actual_enqueue_publishes_captured_native_roots(
        cx: &mut TestAppContext,
    ) {
        enqueue_close_waits_for_native_roots(cx, VerificationCompletion::Release).await;
    }

    #[gpui_kit::test]
    async fn normal_close_retains_capture_and_writer_when_an_actual_enqueue_verification_is_cancelled(
        cx: &mut TestAppContext,
    ) {
        enqueue_close_waits_for_native_roots(cx, VerificationCompletion::Cancel).await;
    }

    async fn enqueue_close_waits_for_native_roots(
        cx: &mut TestAppContext,
        completion: VerificationCompletion,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use openchamber_native_core::send_config::{CatalogModel, CatalogProvider, ModelChoice};
        use openchamber_native_core::session::SessionRecord;
        use openchamber_native_transport::client::ClientDescription;
        use openchamber_native_transport::credential::Credential;
        use openchamber_native_transport::endpoint::Endpoint;

        use crate::attachment_catalog::{PreparationHold, PreparedAttachment};
        use crate::attachment_io::QueueConsumerEditVerifiedCapture;
        use crate::connection::{Connection, ServerConfig};
        use crate::journal_worker::{BlobFailure, ReplyGate};
        use crate::mutation_journal::LoadedJournal;
        use crate::send_catalog::{DirectoryCatalog, Load};

        type VerificationGate = ReplyGate<Result<QueueConsumerEditVerifiedCapture, BlobFailure>>;

        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let (mut journal, _) =
            AttachmentJournal::open(&fixture.journal()).expect("private original journal");
        let owner = seed_companion(&mut journal, "native");
        drop(journal);
        let (mut journal, loaded) =
            AttachmentJournal::open(&fixture.journal()).expect("durable compound fixture");
        let LoadedJournal::Present(mut snapshot) = loaded else {
            panic!("the original companion is committed");
        };
        let blob =
            BlobId::parse("0000000000000000000000000000a001").expect("captured native file label");
        let file_id = AttachmentId::from(blob.as_str());
        let shared_blob = BlobId::parse("0000000000000000000000000000a002")
            .expect("independently owned sibling label");
        let shared_id = AttachmentId::from(shared_blob.as_str());
        let group = AttachmentId::from("close-race-source-group");
        let probe_blob = BlobId::parse("0000000000000000000000000000a003")
            .expect("independent named retirement label");
        let probe_id = AttachmentId::from(probe_blob.as_str());
        let mut holds = vec![blob, shared_blob];
        if matches!(completion, VerificationCompletion::Cancel) {
            holds.push(probe_blob);
        }
        let holds: Vec<_> = holds
            .into_iter()
            .map(|blob| PreparationHold {
                owner: owner.clone(),
                id: AttachmentId::from(blob.as_str()),
                generation: 1,
                blob,
            })
            .collect();
        snapshot.preparations.extend_from_slice(&holds);
        save(&mut journal, &snapshot);
        let mut files = Vec::new();
        for (ordinal, hold) in holds.iter().enumerate() {
            let receipt = journal
                .put(hold, b"original")
                .expect("actual durably held file bytes");
            snapshot.attachments.insert(
                hold.id.clone(),
                PreparedAttachment {
                    blob: receipt.descriptor,
                    staging: None,
                },
            );
            if ordinal < 2 {
                files.push(DraftFile {
                    id: hold.id.clone(),
                    name: Some("removed.txt".into()),
                    media_type: Some("text/plain".into()),
                    byte_len: Some(8),
                    source_document_id: Some(group.clone()),
                    content: DraftFileContent::Prepared,
                });
            }
        }
        snapshot.preparations.clear();
        snapshot
            .drafts
            .iter_mut()
            .find(|(key, _)| key == &owner)
            .expect("current native source")
            .1
            .content
            .files = files.clone();
        let independent_owner = DraftKey::new(
            RuntimeId::from("independent-host"),
            "/other-project",
            DraftTarget::Session(SessionId::from("ses_close_independent")),
        )
        .expect("independent foreign-runtime owner");
        let mut independent = Draft::new("independent original input");
        independent.content.files.push(files[1].clone());
        snapshot
            .drafts
            .push((independent_owner.clone(), independent.clone()));
        let shared_descriptor = snapshot.attachments[&shared_id].blob;
        let removed_descriptor = snapshot.attachments[&file_id].blob;
        save(&mut journal, &snapshot);
        drop(journal);
        let removed_path = fixture
            .journal()
            .parent()
            .expect("private profile")
            .join("attachment-spool")
            .join(format!("blob-{}.bin", blob.as_str()));
        let shared_path = fixture
            .journal()
            .parent()
            .expect("private profile")
            .join("attachment-spool")
            .join(format!("blob-{}.bin", shared_blob.as_str()));
        let probe_path = fixture
            .journal()
            .parent()
            .expect("private profile")
            .join("attachment-spool")
            .join(format!("blob-{}.bin", probe_blob.as_str()));
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(fixture.journal(), window, cx);
            })
        })
        .expect("hydrate the real composer and private file owner");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let mut session = SessionRecord::new(SessionId::from("ses_queue"), "/project");
                session.model = Some(ModelChoice {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    variant: Some("effort".into()),
                });
                let load = workspace.sessions.begin_load();
                let _ = workspace.sessions.complete_load(load, vec![session]);
                workspace.selected = Some(SessionId::from("ses_queue"));
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(Vec::new());
                catalog.providers = Load::Ready(vec![CatalogProvider {
                    id: "provider".into(),
                    name: None,
                    models: vec![CatalogModel {
                        id: "model".into(),
                        provider_model_id: None,
                        name: None,
                        variants: vec!["effort".into()],
                        context_limit: None,
                    }],
                }]);
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory("/project".into(), catalog);
                workspace.send_catalog.settings = Some(Default::default());
                workspace.composer_key = Some(owner.clone());
                workspace.restore_composer_draft(window, cx);
            })
        })
        .expect("restore the actual compound editor");
        cx.condition(&workspace, |workspace, _| {
            workspace.composer_document_ready(&owner)
        })
        .await;
        let initial_flush = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let capture = workspace
                        .capture_composer_submission(cx)
                        .expect("cancel any restoration autosave and capture it exactly");
                    cx.spawn_in(window, async move |this, cx| {
                        composer_document::flush(&this, cx, capture).await
                    })
                })
            })
            .expect("settle pre-existing editing I/O");
        initial_flush
            .await
            .expect("all original editing verification replies are consumed");

        let slot: Rc<RefCell<Option<VerificationGate>>> = Rc::new(RefCell::new(None));
        let gate_slot = slot.clone();
        let (id, observer) = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let existing: HashSet<_> = workspace
                        .queue_storage
                        .objects()
                        .iter()
                        .map(|object| object.id().clone())
                        .collect();
                    let take =
                        QueueOperationId::new("edited_take_original").expect("original edit owner");
                    let observer = cx.observe_self(move |workspace, _| {
                        if gate_slot.borrow().is_some() {
                            return;
                        }
                        if let Some(object) =
                            workspace.queue_storage.objects().iter().find(|object| {
                                object.operation() == &take
                                    && object.kind() == QueueObjectKind::NativeCompanion
                                    && !existing.contains(object.id())
                            })
                        {
                            // AsyncWindowContext flushes notification effects before the
                            // actor can advance from its planned-object save to any put.
                            let consumer = workspace
                                .queue_storage
                                .operations()
                                .iter()
                                .find(|operation| {
                                    operation
                                        .composer_edit()
                                        .is_some_and(|edit| &edit.object == object.id())
                                })
                                .expect("atomic consumer precedes edit puts");
                            let binding = workspace
                                .queue_storage
                                .consumer_edit_binding(consumer.id())
                                .expect("exact live consuming edit binding");
                            let gate = workspace
                                .attachment_worker()
                                .expect("real worker")
                                .hold_consumer_edit_verification_reply_for_test(
                                    binding,
                                    object.verification(),
                                );
                            *gate_slot.borrow_mut() = Some(gate);
                        }
                    });
                    let selection = if workspace.composer.read(cx).selected_range() == (0..1) {
                        0..0
                    } else {
                        0..1
                    };
                    workspace.composer.update(cx, |composer, cx| {
                        composer.set_selected_range(selection, cx)
                    });
                    let endpoint = Endpoint::parse("https://example.invalid/")
                        .expect("passive captured endpoint");
                    let mut connection = Connection::new(ServerConfig {
                        endpoint: endpoint.clone(),
                        password: None,
                        client: ClientDescription::default(),
                    })
                    .expect("owned executor without connection requests");
                    connection.client = Some(
                        ServerClient::new(endpoint, Credential::None).expect("captured client"),
                    );
                    connection.state = ConnectionState::Connected;
                    workspace.connection = Some(connection);
                    workspace.begin_payload_enqueue(window, cx);
                    let actors: Vec<_> = workspace
                        .queue
                        .enqueues
                        .tasks
                        .iter()
                        .filter(|(_, task)| task.owner.as_ref() == Some(&owner))
                        .map(|(id, _)| id.clone())
                        .collect();
                    assert_eq!(
                        actors.len(),
                        1,
                        "the real entrypoint starts its actual captured actor"
                    );
                    (actors[0].clone(), observer)
                })
            })
            .expect("start the actual enqueue and arm its exact verification reply");
        cx.condition(&workspace, |_, _| slot.borrow().is_some())
            .await;
        let mut verification = slot
            .borrow_mut()
            .take()
            .expect("exact actor verification gate");
        drop(observer);
        verification.reached().await;
        let (mut finish, flush_checkpoint) = cx.update_window(window, |_, window, cx| workspace.update(cx, |workspace, cx| {
            assert!(workspace.queue.enqueues.busy(&id) && workspace.queue_storage.operation(&id).is_some());
            assert!(workspace.queue.enqueues.references_native(&file_id) && workspace.queue.enqueues.references_native(&group));
            workspace.remove_composer_attachment(&owner, &file_id, window, cx);
            assert!(workspace.drafts.get(&owner).expect("current draft").content.files.is_empty());
            assert!(workspace.attachments.contains_key(&file_id) && !workspace.has_pending_attachment_retirements(),
                "the live actor still protects removal, so this is not a pre-injected pending request");
            let finish = workspace.attachment_worker().expect("live journal owner").hold_finish_reply_for_test();
            workspace.request_close(window, cx);
            let checkpoint = workspace.attachment_worker().expect("close waits without stopping its writer").flush();
            (finish, checkpoint)
        })).expect("remove through the real handler and request a normal close");

        assert_eq!(
            flush_checkpoint
                .await
                .expect("real worker flush answers while the actor reply is held"),
            Ok(CommitOutcome::Durable)
        );
        cx.run_until_parked();
        assert!(
            verification.has_reached() && !finish.has_reached(),
            "close cannot finish ahead of the actor's captured roots"
        );
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(
                    workspace.close_in_progress()
                        && !workspace.persistence_closing()
                        && !workspace.composer_editable()
                );
                assert!(
                    workspace.native_root_actors_pending()
                        && workspace.queue.enqueues.has_native_root_holds()
                );
                assert!(
                    workspace.queue.enqueues.references_native(&file_id)
                        && workspace.queue.enqueues.references_native(&group)
                );
                assert!(
                    !workspace.has_pending_attachment_retirements()
                        && workspace.attachments.contains_key(&file_id)
                );
                assert!(
                    removed_path.is_file() && shared_path.is_file(),
                    "close preserves every actor-protected byte"
                );
                let actors = workspace.queue.enqueues.tasks.len();
                workspace.begin_payload_enqueue(window, cx);
                workspace.request_close(window, cx);
                assert_eq!(
                    workspace.queue.enqueues.tasks.len(),
                    actors,
                    "the close phase blocks new admission and preserves existing actor tasks"
                );
                assert!(
                    workspace.queue.enqueues.busy(&id)
                        && workspace.queue_storage.operation(&id).is_some()
                );
                assert_eq!(workspace.drafts.get(&independent_owner), Some(&independent));
            })
        })
        .expect("normal close keeps the window alive while the actual actor is held");
        assert!(
            matches!(
                AttachmentJournal::open(&fixture.journal()),
                Err(JournalError::Locked)
            ),
            "waiting close retains the real journal lock"
        );
        match completion {
            VerificationCompletion::Release => verification.release(),
            VerificationCompletion::Cancel => verification.cancel(),
        }
        cx.condition(&workspace, |workspace, _| {
            !workspace.queue.enqueues.busy(&id)
        })
        .await;
        if matches!(completion, VerificationCompletion::Cancel) {
            cx.condition(&workspace, |workspace, _| !workspace.close_in_progress())
                .await;
        } else {
            finish.reached().await;
        }
        workspace.update(cx, |workspace, cx| {
            assert!(!workspace.native_root_actors_pending());
            let operation = workspace
                .queue_storage
                .operation(&id)
                .expect("atomic consumer retains the original capture");
            assert!(
                matches!(
                    operation.action(),
                    QueueAction::Enqueue {
                        phase: QueueEnqueuePhase::Capturing,
                        ..
                    }
                ),
                "normal close cannot advance preparation, uploads or dispatch"
            );
            let preparation = operation
                .composer_preparation()
                .expect("original pristine marker");
            assert!(
                preparation.sources.is_empty() && preparation.inputs_object().is_none(),
                "close grants local capture completion only"
            );
            assert!(
                workspace.attachment_has_owner(&file_id)
                    && workspace.attachments.contains_key(&file_id)
            );
            assert!(
                !workspace.has_pending_attachment_retirements(),
                "consumer-owned files cannot become unowned retirement work"
            );
            assert!(
                workspace
                    .drafts
                    .get(&owner)
                    .expect("user-removed input stays removed")
                    .content
                    .files
                    .is_empty()
            );
            assert_eq!(
                workspace.composer.read(cx).value().as_ref(),
                "native\n\n#snippet"
            );
            assert!(
                workspace.attachments.contains_key(&shared_id)
                    && workspace.attachment_has_owner(&shared_id)
            );
            assert!(!workspace.retired_attachment_blobs.contains(&shared_blob));
            assert_eq!(workspace.drafts.get(&independent_owner), Some(&independent));
        });
        if matches!(completion, VerificationCompletion::Cancel) {
            assert!(
                !finish.has_reached(),
                "a cancelled local completion cannot seal worker admission"
            );
            assert!(
                cx.update_window(window, |_, _, _| ()).is_ok(),
                "failed local custody retains the window"
            );
            let (stamp, saved) = workspace.update(cx, |workspace, cx| {
                workspace
                    .persistence_barrier(cx)
                    .expect("close refusal retains a usable writer")
            });
            let saved = saved
                .await
                .expect("writer remains able to save after close refusal");
            assert!(
                workspace.update(cx, |workspace, cx| workspace
                    .record_persistence(stamp, &saved, cx)),
                "close refusal preserves write authority"
            );
            assert!(
                matches!(
                    AttachmentJournal::open(&fixture.journal()),
                    Err(JournalError::Locked)
                ),
                "close refusal retains the exclusive journal lock"
            );
            // An unrelated named release still has independent failure and retry
            // coverage. Actual unread worker receipts exhaust byte-job admission;
            // no global writer permission or captured consumer root is cleared.
            let readers = workspace.update(cx, |workspace, _| {
                (0..64)
                    .map(|_| {
                        workspace
                            .attachment_worker()
                            .expect("retained writer")
                            .read_attachment(shared_descriptor)
                    })
                    .collect::<Vec<_>>()
            });
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.retire_attachment(probe_id.clone(), window, cx)
                })
            })
            .expect("request an independent actual named retirement");
            cx.condition(&workspace, |workspace, _| {
                !workspace.attachment_retirement.busy(&probe_blob)
            })
            .await;
            workspace.read_with(cx, |workspace, _| {
                // The failed release keeps its journaled intent without blocking close.
                assert!(
                    !workspace.has_pending_attachment_retirements()
                        && workspace.retired_attachment_blobs.contains(&probe_blob)
                );
                assert!(probe_path.is_file() && removed_path.is_file() && shared_path.is_file());
                assert!(
                    workspace.attachment_has_owner(&file_id),
                    "failed independent release cannot weaken captured custody"
                );
            });
            drop(readers);
            let drained = workspace.update(cx, |workspace, _| {
                workspace
                    .attachment_worker()
                    .expect("same retained writer")
                    .flush()
            });
            assert_eq!(
                drained.await.expect("accepted unread reads drain"),
                Ok(CommitOutcome::Durable)
            );
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.retry_attachment_retirements(window, cx)
                })
            })
            .expect("explicitly retry only the independent named retirement");
            cx.condition(&workspace, |workspace, _| {
                !workspace.has_pending_attachment_retirements()
                    && !workspace.attachment_retirement.busy(&probe_blob)
            })
            .await;
            assert!(
                !probe_path.exists() && removed_path.is_file() && shared_path.is_file(),
                "only the explicitly retired unowned file is unlinked"
            );
            let (current_text, current_selection, current_history, current_native, current_companions) = workspace.update(cx, |workspace, cx| {
            assert!(!workspace.close_in_progress() && workspace.persistence_ready() && !workspace.native_root_actors_pending());
            assert!(workspace.composer_document_save_failed(&owner) && workspace.composer_documents_dirty(),
                "the task-independent registration retains the exact failed edit for explicit Retry");
            assert!(!workspace.composer_documents_pending() && workspace.composer_document_ready(&owner),
                "submission failure leaves independent current input available");
            assert!(!finish.has_reached() && !workspace.has_pending_attachment_retirements());
            assert!(workspace.drafts.get(&owner).expect("user input survives the refused close").content.files.is_empty());
            let composer = workspace.composer.read(cx);
            assert_eq!(composer.value().as_ref(), "native\n\n#snippet", "the refused close preserves current input");
            (composer.value().to_string(), composer.selected_range(), composer.history_checkpoint(),
                workspace.drafts.get(&owner).expect("current native input").clone(), workspace.queue_storage.draft_companions(&owner))
        });
            assert!(
                cx.update_window(window, |_, _, _| ()).is_ok(),
                "a failed editing save retains the window for explicit Retry"
            );
            assert!(
                matches!(
                    AttachmentJournal::open(&fixture.journal()),
                    Err(JournalError::Locked)
                ),
                "editing-save refusal retains the real writer"
            );
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.retry_composer_document(&owner, window, cx);
                    assert!(
                        workspace.composer_documents_pending(),
                        "the existing Retry action starts the retained editing save"
                    );
                })
            })
            .expect(
                "explicitly retry the exact failed editing snapshot through its existing action",
            );
            cx.condition(&workspace, |workspace, _| {
                !workspace.composer_documents_pending()
            })
            .await;
            workspace.update(cx, |workspace, cx| {
            assert!(!workspace.composer_document_save_failed(&owner), "the existing explicit Retry left a retained editing-save failure");
            assert!(workspace.composer_document_ready(&owner) && !workspace.composer_documents_dirty(), "the exact failed edit and current revision are durably saved");
            assert!(!workspace.close_in_progress() && workspace.persistence_ready() && !finish.has_reached());
            assert_eq!(workspace.composer.read(cx).value().as_ref(), current_text.as_str(), "Retry preserves the current textarea input");
            assert_eq!(workspace.composer.read(cx).selected_range(), current_selection, "Retry preserves the current selection");
            assert_eq!(workspace.composer.read(cx).history_checkpoint(), current_history, "Retry preserves the current undo history");
            assert_eq!(workspace.drafts.get(&owner), Some(&current_native), "Retry cannot replace native input with the actor's older capture");
            assert_eq!(workspace.queue_storage.draft_companions(&owner), current_companions, "Retry preserves every current companion owner");
            assert!(workspace.attachments.contains_key(&file_id) && workspace.attachment_has_owner(&file_id) && removed_path.is_file(),
                "local Retry preserves the captured consumer's bytes without restoring the removed current file");
            assert!(!workspace.has_pending_attachment_retirements() && !workspace.retired_attachment_blobs.contains(&blob));
            assert_eq!(workspace.drafts.get(&independent_owner), Some(&independent));
            assert!(workspace.attachments.contains_key(&shared_id) && workspace.attachment_has_owner(&shared_id) && shared_path.is_file());
        });
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.request_close(window, cx);
                    assert!(
                        workspace.close_in_progress(),
                        "a recovered and fully saved editor admits normal close"
                    );
                })
            })
            .expect("close normally after the explicit editing Retry succeeds");
            // The released path already observed the gated finish above.
            finish.reached().await;
        }
        let (journal, _) = AttachmentJournal::open(&fixture.journal())
            .expect("the gated finish has dropped its actual writer and released the lock");
        drop(journal);
        finish.release();
        cx.run_until_parked();
        assert!(
            cx.update_window(window, |_, _, _| ()).is_err(),
            "a clean retry completes normal window close"
        );
        let (journal, loaded) = AttachmentJournal::open(&fixture.journal())
            .expect("restart reads the final authoritative state");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("the final normal-close snapshot is durable");
        };
        assert_eq!(
            restored.attachments.get(&file_id).map(|entry| entry.blob),
            Some(removed_descriptor)
        );
        assert!(
            !restored.retired_blobs.contains(&blob) && removed_path.is_file(),
            "the typed consumer retains restart-readable captured roots"
        );
        assert_eq!(
            journal
                .read(&removed_descriptor)
                .expect("captured removed file remains readable")
                .as_slice(),
            b"original"
        );
        let operation = restored
            .queue
            .operation(&id)
            .expect("normal close preserves the original Capturing consumer");
        let edit = operation
            .composer_edit()
            .expect("exact original edit reference");
        assert!(
            restored
                .queue
                .object(&edit.object)
                .is_some_and(|object| object.verified())
        );
        let Some(QueueSource::Companion(source)) = operation.source() else {
            panic!("normal close publishes the original source locally");
        };
        assert!(
            restored
                .queue
                .object(source)
                .is_some_and(|object| object.verified())
        );
        assert_eq!(
            restored.attachments.get(&shared_id).map(|entry| entry.blob),
            Some(shared_descriptor)
        );
        if matches!(completion, VerificationCompletion::Cancel) {
            assert!(
                !restored.attachments.contains_key(&probe_id)
                    && !restored.retired_blobs.contains(&probe_blob)
                    && !probe_path.exists()
            );
        }
        assert_eq!(
            journal
                .read(&shared_descriptor)
                .expect("independent foreign-runtime sibling remains readable")
                .as_slice(),
            b"original"
        );
        assert!(shared_path.is_file());
        assert!(
            restored
                .drafts
                .iter()
                .find(|(key, _)| key == &owner)
                .expect("original owner survives restart")
                .1
                .content
                .files
                .is_empty()
        );
        assert_eq!(
            restored
                .drafts
                .iter()
                .find(|(key, _)| key == &independent_owner)
                .map(|(_, draft)| draft),
            Some(&independent)
        );
    }

    #[gpui_kit::test]
    async fn actual_compound_edit_capture_retains_source_literal_layout_references_and_newer_input(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let (mut journal, _) =
            AttachmentJournal::open(&fixture.journal()).expect("private journal");
        let owner = seed_companion(&mut journal, "native");
        drop(journal);
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.runtime = owner.runtime().clone();
                workspace.open_persistence(fixture.journal(), window, cx);
            })
        })
        .expect("real local storage owner");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = Some(owner.clone());
                workspace.restore_composer_draft(window, cx);
            })
        })
        .expect("restore the actual compound composer");
        cx.condition(&workspace, |workspace, _| {
            workspace.composer_document_ready(&owner)
        })
        .await;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert_eq!(
                    workspace.composer.read(cx).value().as_ref(),
                    "native\n\n#snippet"
                );
                workspace.composer.update(cx, |composer, cx| {
                    composer.focus(window, cx);
                    composer.set_selected_range(6..8, cx);
                });
            });
            window.render_frame(cx);
            // Window::input dispatches one character at a time. Once its first
            // space replaces the separator, later characters belong to the next
            // source. One real replacement captures the intended literal edit.
            workspace.update(cx, |workspace, cx| {
                workspace.composer.update(cx, |composer, cx| {
                    composer.replace_text_in_range(None, " | ", window, cx);
                })
            });
        })
        .expect("one committed textarea replacement edits the captured separator");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .composer
                    .update(cx, |composer, cx| composer.set_selected_range(9..17, cx))
            });
            window.input("edited λ", cx);
        })
        .expect("ordinary editing changes only the companion source");
        cx.run_until_parked();
        let captured = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let captured = workspace
                        .capture_composer_submission(cx)
                        .expect("real synchronous compound submission");
                    assert_eq!(
                        captured.composer.native.draft.text, "native",
                        "joined editing text never replaces the original native draft"
                    );
                    assert_eq!(captured.composer.references.len(), 1);
                    cx.spawn_in(window, async move |this, cx| {
                        let captured = composer_document::flush(&this, cx, captured).await?;
                        let materialized =
                            composer_document::materialize(&this, cx, &captured).await?;
                        Ok::<_, QueueCaptureFailure>((captured, materialized))
                    })
                })
            })
            .expect("flush and materialize the real captured edit")
            .await
            .expect("verified local source and editing bytes");
        let (captured, inputs) = captured;
        assert!(
            captured.edit.is_some(),
            "the compound capture retains its exact published editing snapshot"
        );
        assert_eq!(inputs.native.draft.text, "native");
        assert!(
            matches!(&inputs.text[..], [crate::composer_document::ComposerTextPiece::Source(0),
            crate::composer_document::ComposerTextPiece::Literal { text, edited: true }, crate::composer_document::ComposerTextPiece::Source(1)] if text == " | "),
            "the exact Source/Literal layout survives the durable edit round trip"
        );
        let [
            crate::composer_document::ComposerCompanionInput::Edited {
                capture,
                files,
                reprocess_text,
            },
        ] = &inputs.companions[..]
        else {
            panic!("the edited companion has a genuine typed local capture");
        };
        assert!(*reprocess_text);
        assert_eq!(capture.item().content, "edited λ");
        assert!(files.is_empty() && capture.item().files.is_empty());
        assert_eq!(
            capture.item().prompt_parts.file_references[0].uri,
            "file:///project/reference.pdf"
        );
        assert_eq!(capture.context_ids(), [MessageId::from("msg_original")]);
        assert_eq!(capture.item().prompt_parts.skills[0].id, "captured-skill");
        let native_input = QueueLocalCapture::new(
            "/project",
            QueueLocalItem {
                content: "native".into(),
                text: "native".into(),
                files: Vec::new(),
                send_config: configuration(),
                prompt_parts: QueuePromptParts::default(),
            },
            Vec::new(),
            sha256,
        )
        .expect("captured native preparation input");
        let source_inputs = vec![
            QueuePreparationSource {
                identity: QueuePreparationSourceId::Source(0),
                byte_len: native_input.bytes().len() as u64,
                sha256: native_input.sha256(),
            },
            QueuePreparationSource {
                identity: QueuePreparationSourceId::Source(1),
                byte_len: capture.bytes().len() as u64,
                sha256: capture.sha256(),
            },
        ];
        let prepared_local = QueueLocalCapture::new(
            "/project",
            QueueLocalItem {
                content: "native | edited λ".into(),
                text: "native | edited λ".into(),
                files: Vec::new(),
                send_config: configuration(),
                prompt_parts: capture.item().prompt_parts.clone(),
            },
            capture.context_ids().to_vec(),
            sha256,
        )
        .expect("already projected typed input with all original companion fields");
        let old_edit = captured.edit.as_ref().expect("original edit").clone();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .composer
                    .update(cx, |composer, cx| composer.set_selected_range(18..18, cx))
            });
            window.input(" newer", cx);
        })
        .expect("independent newer typing");
        cx.run_until_parked();
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.save_composer_draft(cx);
                let newer = workspace
                    .capture_composer_submission(cx)
                    .expect("independent newer whole input");
                assert!(newer.revision > captured.revision);
                assert!(captured.edit.as_ref() == Some(&old_edit));
                assert_eq!(
                    workspace.composer.read(cx).value().as_ref(),
                    "native | edited λ newer"
                );
            })
        })
        .expect("later typing cannot replace the immutable submission");
        let enqueue =
            QueueOperationId::new("real_edited_enqueue").expect("exact operation identity");
        let captured_for_operation = captured.clone();
        let source = workspace.update(cx, |workspace, _| {
            let owner = QueueOwner::new(
                owner.runtime(),
                "/project",
                &SessionId::from("ses_queue"),
                &owner,
            )
            .expect("captured queue owner");
            let mut operation = QueueOperation::enqueue_with_edit(
                enqueue.clone(),
                owner,
                QueueConfiguration::capture(&configuration()),
                captured_for_operation.composer.clone(),
                Vec::new(),
                Vec::new(),
                captured_for_operation.edit.clone(),
            )
            .expect("real original edit admission");
            operation
                .begin_composer_preparation(
                    captured_for_operation.generation,
                    captured_for_operation.revision,
                )
                .expect("original marker");
            let source = operation
                .enqueue_capture()
                .expect("exact typed original source");
            workspace
                .change_queue_storage(|queue, workspace| {
                    queue
                        .admit_operation(operation, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                        .map_err(|(error, _)| error)
                })
                .expect("whole original source enters the ledger");
            source
        });
        let captured_operation = enqueue.clone();
        let source_bytes = serde_json::to_vec(&source).expect("exact original typed source bytes");
        let source_sha256 = sha256(&source_bytes);
        let generation = captured.generation;
        let revision = captured.revision;
        let prepared = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        queue_payload_io::persist(
                            &this,
                            cx,
                            &captured_operation,
                            "source",
                            QueueObjectKind::NativeCompanion,
                            source_bytes,
                            true,
                        )
                        .await?;
                        this.update_in(cx, |workspace, _, _| {
                            let plan = QueueComposerPreparation::new(
                                &owner,
                                source_sha256,
                                generation,
                                revision,
                                source_inputs,
                                Vec::new(),
                                Vec::new(),
                            );
                            let mut queue = std::mem::take(&mut workspace.queue_storage);
                            let mut preparations =
                                std::mem::take(&mut workspace.attachment_preparations);
                            let result = queue.plan_composer_preparation(
                                &captured_operation,
                                plan,
                                &workspace.attachments,
                                &mut preparations,
                                |_, _| Ok(()),
                                |candidate, _| workspace.validate_queue_candidate(candidate),
                            );
                            workspace.queue_storage = queue;
                            workspace.attachment_preparations = preparations;
                            result
                        })
                        .map_err(|_| QueueCaptureFailure::Unavailable)??;
                        queue_payload_io::save(&this, cx).await?;
                        let input = PreparedComposerInput::new(
                            prepared_local,
                            Vec::new(),
                            generation,
                            revision,
                            source_sha256,
                        )?;
                        composer_payload::persist_and_seal(
                            &this,
                            cx,
                            &captured_operation,
                            &input,
                            false,
                        )
                        .await?;
                        let document = QueuePayloadDocument::from_manifest(
                            "/project",
                            compound_manifest(&input.capture, Vec::new())?,
                            QueuePayloadUse::Enqueue,
                            sha256,
                        )?;
                        let payload = queue_payload_io::persist(
                            &this,
                            cx,
                            &captured_operation,
                            "payload",
                            QueueObjectKind::Payload,
                            document.bytes().to_vec(),
                            false,
                        )
                        .await?;
                        let descriptor = QueueAttachmentDescriptor {
                            blob_id: [0xab; 32],
                            request_id: captured_operation.as_str().to_owned(),
                            attachment_id: "payload-0".into(),
                            filename: "payload-0.json".into(),
                            mime_type: "application/json".into(),
                            size: document.bytes().len() as u64,
                            sha256: document.sha256(),
                        };
                        let client = ServerClient::new(
                            openchamber_native_transport::endpoint::Endpoint::parse(
                                "https://example.invalid/",
                            )
                            .expect("inert endpoint"),
                            openchamber_native_transport::credential::Credential::ClientToken(
                                "synthetic-enqueue-test-token".into(),
                            ),
                        )
                        .expect("protocol-only captured client");
                        let scope =
                            QueueScope::new(&client, SessionId::from("ses_queue"), "/project")?;
                        let enqueue = QueuePayloadEnqueue::new(
                            scope.request(captured_operation.as_str())?,
                            document.pointer(vec![descriptor], sha256)?,
                            &document,
                            sha256,
                        )?;
                        // This descriptor validates the request only. The test makes no HTTP
                        // upload and never treats it as a Take or acknowledgement proof.
                        Ok::<_, QueueCaptureFailure>((
                            payload,
                            QueueJson::new(enqueue.body().to_vec())?,
                        ))
                    })
                })
            })
            .expect("persist exact source, prepared input and final payload")
            .await
            .expect("durable compound preparation");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let checkpoint = workspace.composer.read(cx).history_checkpoint();
                let remaining = workspace
                    .queue_storage
                    .draft_companions(&captured.composer.native.owner);
                workspace
                    .change_queue_storage(|queue, workspace| {
                        queue.prepare_enqueue(&enqueue, prepared.0, prepared.1, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                    })
                    .expect("only durable payloads enter Ready");
                workspace
                    .consume_composer_submission(&enqueue, &captured, window, cx)
                    .expect("the prepared enqueue consumes by exact revision");
                assert_eq!(
                    workspace.composer.read(cx).value().as_ref(),
                    "native | edited λ newer"
                );
                assert_eq!(
                    workspace.composer.read(cx).history_checkpoint(),
                    checkpoint,
                    "newer input retains its toolkit Undo history"
                );
                assert_eq!(
                    workspace
                        .queue_storage
                        .draft_companions(&captured.composer.native.owner),
                    remaining,
                    "newer whole input retains every original rich root"
                );
                assert!(matches!(
                    workspace
                        .queue_storage
                        .operation(&enqueue)
                        .expect("original prepared operation")
                        .action(),
                    QueueAction::Enqueue {
                        phase: QueueEnqueuePhase::Ready { .. },
                        ..
                    }
                ));
            })
        })
        .expect("prepared consumption preserves newer compound input and all of its roots");
        let (stamp, saved) = workspace.update(cx, |workspace, cx| {
            workspace
                .persistence_barrier(cx)
                .expect("complete prepared-consumption barrier")
        });
        let saved = saved.await.expect("final durable snapshot answer");
        assert!(
            workspace.update(cx, |workspace, cx| workspace
                .record_persistence(stamp, &saved, cx)),
            "the prepared enqueue and preserved newer input commit together"
        );
        let finish = workspace.update(cx, |workspace, _| {
            workspace.attachment_worker().expect("real worker").finish()
        });
        assert_eq!(
            finish.await.expect("worker drain"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("close the test-owned window");
    }
}

#[cfg(unix)]
mod unpublished_chunk_recovery {
    use super::*;

    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::attachment_io::AttachmentFailure;
    use crate::attachment_spool::{SpoolCommit, SpoolError};
    use crate::journal_worker::{BlobFailure, CommitOutcome, JournalWorker};
    use crate::mutation_journal::{JournalSnapshot, LoadedJournal};
    use crate::queue_capture::{QueueCaptureBytes, QueueObjectId};
    use crate::queue_storage::QueueObject;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_retry_resumes_only_the_exact_committed_unpublished_chunk() {
        let scratch = Scratch(PathBuf::from(format!(
            "/tmp/opencode/enqueue-repair-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&scratch.0)
            .expect("private test profile");
        let journal = scratch.0.join("journal.json");
        let (worker, loaded) = JournalWorker::start(journal.clone());
        assert!(matches!(
            loaded
                .blocking_recv()
                .expect("worker answers")
                .expect("journal opens"),
            LoadedJournal::Missing
        ));
        let mut original = native();
        original.draft.content.files.clear();
        let operation =
            QueueOperationId::new("enqueue_repair_original").expect("stable request identity");
        let object =
            QueueObjectId::new("enqueue_repair_original-source").expect("stable source object");
        let blob = BlobId::parse("00000000000000000000000000000101").expect("local-only label");
        let captured =
            QueueCaptureBytes::native_source(&original, &[blob]).expect("complete captured source");
        let mut snapshot = JournalSnapshot::default();
        snapshot
            .drafts
            .push((original.owner.clone(), original.draft.clone()));
        let owner = QueueOwner::new(
            original.owner.runtime(),
            "/project",
            &SessionId::from("ses_queue"),
            &original.owner,
        )
        .expect("original owner");
        assert!(
            snapshot
                .queue
                .admit_operation(
                    QueueOperation::enqueue(
                        operation.clone(),
                        owner,
                        QueueConfiguration::capture(&configuration()),
                        original.clone()
                    )
                    .expect("captured operation"),
                    |_| Ok(())
                )
                .is_ok()
        );
        assert!(
            snapshot
                .queue
                .admit_object(
                    QueueObject::planned(
                        object.clone(),
                        operation.clone(),
                        QueueObjectKind::NativeCompanion,
                        1,
                        captured.reference().clone()
                    )
                    .expect("source hold"),
                    |_| Ok(())
                )
                .is_ok()
        );
        let hold = snapshot
            .queue
            .object(&object)
            .expect("source object")
            .hold(0)
            .expect("unpublished hold");
        assert_eq!(
            worker
                .resume_queue_chunk(hold.clone(), captured.bytes().to_vec())
                .blocking_recv()
                .expect("refusal answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized)),
            "memory-only holds never authorize repair"
        );
        assert_eq!(
            worker
                .submit(snapshot.clone())
                .blocking_recv()
                .expect("save answers"),
            Ok(CommitOutcome::Durable)
        );
        let path = scratch
            .0
            .join("attachment-spool")
            .join(format!("blob-{}.bin", blob.as_str()));
        // A failed write can leave this exact private prefix under the saved hold.
        let prefix = &captured.bytes()[..17];
        let mut partial = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("partial spool file");
        partial
            .write_all(prefix)
            .expect("the surviving partial write");
        partial.sync_all().expect("persist failure artifact");
        drop(partial);
        let inode = fs::metadata(&path).expect("partial inode").ino();
        assert_eq!(
            worker
                .put_queue_chunk(hold.clone(), captured.bytes().to_vec())
                .blocking_recv()
                .expect("put answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Collision
            ))),
            "ordinary put reproduces the trapped label"
        );
        let mut stale = hold.clone();
        stale.generation += 1;
        assert_eq!(
            worker
                .resume_queue_chunk(stale, captured.bytes().to_vec())
                .blocking_recv()
                .expect("stale repair answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        assert_eq!(fs::read(&path).expect("partial remains"), prefix);
        let mut refused = snapshot.clone();
        refused.drafts[0].1.content.metadata_json = Some("[]".into());
        assert!(
            worker
                .submit(refused)
                .blocking_recv()
                .expect("rejected snapshot answers")
                .is_err()
        );
        assert_eq!(
            worker
                .resume_queue_chunk(hold.clone(), captured.bytes().to_vec())
                .blocking_recv()
                .expect("revoked repair answers"),
            Err(BlobFailure::Attachment(
                AttachmentFailure::NoDurableAuthority
            )),
            "a failed save revokes the former hold authority"
        );
        assert_eq!(
            fs::read(&path).expect("revoked repair keeps partial bytes"),
            prefix
        );
        assert_eq!(
            worker
                .submit(snapshot.clone())
                .blocking_recv()
                .expect("explicit authority save answers"),
            Ok(CommitOutcome::Durable)
        );
        let receipt = worker
            .resume_queue_chunk(hold.clone(), captured.bytes().to_vec())
            .blocking_recv()
            .expect("explicit repair answers")
            .expect("matching partial bytes resume");
        assert_eq!(receipt.commit, SpoolCommit::Durable);
        assert_eq!(
            fs::metadata(&path).expect("resumed inode").ino(),
            inode,
            "repair appends without deleting or replacing the chunk"
        );
        assert_eq!(
            fs::read(&path).expect("complete spool bytes"),
            captured.bytes()
        );
        snapshot
            .queue
            .record_chunk(&hold, &receipt)
            .expect("retain exact durable receipt");
        assert_eq!(
            worker
                .submit(snapshot.clone())
                .blocking_recv()
                .expect("receipt save answers"),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(
            worker
                .resume_queue_chunk(hold, captured.bytes().to_vec())
                .blocking_recv()
                .expect("published repair answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized)),
            "a durable chunk receipt revokes repair authority"
        );
        let proof = worker
            .verify_queue_capture(
                snapshot
                    .queue
                    .object(&object)
                    .expect("original source")
                    .verification(),
            )
            .blocking_recv()
            .expect("verification answers")
            .expect("whole source matches");
        snapshot
            .queue
            .publish_source(&operation, &proof)
            .expect("publish original complete source");
        assert_eq!(
            worker
                .submit(snapshot)
                .blocking_recv()
                .expect("publication save answers"),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(
            worker.finish().blocking_recv().expect("drain answers"),
            Ok(CommitOutcome::Durable)
        );
        let (worker, loaded) = JournalWorker::start(journal);
        let LoadedJournal::Present(restored) = loaded
            .blocking_recv()
            .expect("restart answers")
            .expect("restart loads")
        else {
            panic!("saved original owner");
        };
        assert_eq!(
            restored.drafts[0].1, original.draft,
            "repair preserves the complete captured composer source"
        );
        assert!(
            matches!(restored.queue.operation(&operation).expect("same request identity").action(),
            QueueAction::Enqueue { source: QueueSource::Companion(saved), .. } if saved == &object)
        );
        assert_eq!(
            worker
                .finish()
                .blocking_recv()
                .expect("restart drain answers"),
            Ok(CommitOutcome::Durable)
        );
    }
}
