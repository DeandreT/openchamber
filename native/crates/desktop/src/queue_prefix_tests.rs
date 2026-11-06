use super::*;

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::draft_content::{
    AgentMention, DraftContext, DraftFile, Mention, SkillRef,
};
use openchamber_native_core::ids::ContextItemId;
use openchamber_native_core::message_queue::{AttachmentSource, QueuedAttachment};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::queue_manifest::{
    QueueFileReference, QueuePayloadChunk, QueuePayloadItem, QueuePayloadPointer,
    QueuePayloadTakenItem, QueueScope,
};
use serde_json::json;

fn configuration() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "model".into(),
        agent: Some("build".into()),
        variant: Some("high".into()),
    }
}

fn source() -> PrefixSourceCapture {
    let owner = DraftKey::new(
        RuntimeId::from("original-host"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_prefix")),
    )
    .expect("captured native owner");
    let mut draft = Draft::new("👩 @build");
    draft.content.files.push(DraftFile {
        id: AttachmentId::from(format!("{:032x}", 1)),
        name: Some("native.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(8),
        source_document_id: Some(AttachmentId::from("source-group-original")),
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
        id: "native_skill".into(),
    });
    draft.content.metadata_json = Some(r#"{"native":true}"#.into());
    draft.content.context.push(DraftContext {
        id: ContextItemId::from("native-chip-original"),
        text: "native context".into(),
        description: Some("native description".into()),
        metadata_json: Some(r#"{"nativeContext":true}"#.into()),
        instructions: Some("native instructions".into()),
    });
    let composer = CompanionCapture {
        native: crate::queue_capture::QueueNativeSource { owner, draft },
        references: Vec::new(),
    };
    let intent = NativePrefixIntent {
        kind: NativePrefixKind::Capture,
        directory: "/project".into(),
        id: "msg_prefix_original".into(),
        item_ids: vec!["q_first".into(), "q_second".into()],
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
    PrefixSourceCapture {
        version: 1,
        composer,
        intent,
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A schema-ordered backend fixture. Production parsing and SHA-256 still verify
/// every pointer and index seal; previews never regenerate the sealed bytes.
fn materialized(
    item: Value,
    context_ids: &[&str],
    roots: &[[u8; 32]],
    legacy_ids: Option<&[&str]>,
    entries: usize,
) -> MaterializedQueueItem {
    let bytes =
        serde_json::to_vec(&json!({"directory":"/project","item":item})).expect("capture bytes");
    let document =
        QueuePayloadDocument::from_bytes(bytes, "/project", QueuePayloadUse::Composer, sha256)
            .expect("complete typed capture");
    let length = document.bytes().len();
    let digest = hex(&document.sha256());
    let pointer = format!(
        r#"{{"version":1,"encoding":"utf-8-json","totalBytes":{length},"sha256":"{digest}","chunks":[{{"attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":{length},"sha256":"{digest}","blobId":"{}","requestId":"original_payload_request","ordinal":0}}]}}"#,
        "cd".repeat(32)
    );
    let ids = serde_json::to_string(context_ids).expect("frozen context IDs");
    let roots = serde_json::to_string(&roots.iter().map(hex).collect::<Vec<_>>())
        .expect("original blob roots");
    let legacy = legacy_ids.map_or_else(String::new, |ids| {
        format!(
            ",\"legacyAttachmentIds\":{}",
            serde_json::to_string(ids).expect("legacy IDs")
        )
    });
    let index = format!(
        r#"{{"directory":"/project","sendConfig":{{"providerID":"provider","modelID":"model","agent":"build","variant":"high"}},"preview":"{}","entryCount":{entries},"contextIds":{ids},"attachmentBlobIds":{roots},"promptBytes":128,"admissionBytes":256,"structuredBytes":0{legacy}}}"#,
        match document.item() {
            QueuePayloadItem::Manifest(item) => item.content.as_str(),
            QueuePayloadItem::Legacy(item) => item.content.as_str(),
        }
    );
    let binding = format!(r#"{{"payloadManifest":{pointer},"payloadIndex":{index}}}"#);
    let seal = hex(&sha256(binding.as_bytes()));
    let body = format!(
        r#"{{"format":"payload-v1","payloadManifest":{pointer},"payloadIndex":{index},"payloadIndexHash":"{seal}"}}"#
    );
    let indexed = QueueIndexedPayload::from_body(body.as_bytes(), "/project", sha256)
        .expect("original sealed index");
    let document = indexed
        .materialize(document.bytes().to_vec(), QueuePayloadUse::Composer, sha256)
        .expect("genuine sealed materialization");
    let proof = document.files_proof(sha256).expect("original file proofs");
    let files = proof
        .files()
        .iter()
        .enumerate()
        .map(|(ordinal, file)| {
            let local = format!("{:032x}", ordinal + 100);
            match file.source() {
                QueuePayloadFileSource::Manifest(blob) => QueueFileBinding::Manifest {
                    blob: *blob,
                    local,
                    byte_len: file.byte_len(),
                    sha256: *file.sha256(),
                },
                QueuePayloadFileSource::Legacy(index) => QueueFileBinding::Legacy {
                    ordinal: *index,
                    local,
                    byte_len: file.byte_len(),
                    sha256: *file.sha256(),
                },
            }
        })
        .collect();
    MaterializedQueueItem::from_materialized(
        QueueTransferItem {
            item: "q_original".into(),
            index: QueueJson::new(indexed.body().expect("exact index bytes"))
                .expect("bounded metadata"),
            payload: QueueObjectId::new("original-capture").expect("spool reference"),
            files,
        },
        indexed,
        document,
    )
    .expect("materialized original authority")
}

fn companion() -> MaterializedQueueItem {
    materialized(
        json!({"format":"manifest-v1","content":"@review $skill @file display","text":"@review $skill @file delivered",
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"},
        "attachmentManifest":[{"blobId":"ab".repeat(32),"requestId":"original_file_owner","attachmentId":"file-0",
            "filename":"companion.txt","mimeType":"text/plain","size":2,"sha256":hex(&sha256(b"hi")),
            "description":"original file description","mention":{"start":15,"end":20,"text":"@file"}}],
        "promptParts":{"agents":[{"name":"review","mention":{"start":0,"end":7,"text":"@review"}}],
            "skills":[{"id":"companion_skill","mention":{"start":8,"end":14,"text":"$skill"}}],"metadata":{"queued":true},
            "context":[{"id":"msg_companion_original","text":"queued context","description":"original description","metadata":{"queuedContext":true}}]}}),
        &["msg_companion_original"],
        &[[0xab; 32]],
        None,
        4,
    )
}

fn handoff() -> QueuePrefixHandoff {
    QueuePrefixHandoff {
        receipt: QueueJson::new(b"{}".to_vec())
            .expect("compact placeholder for pure state planner"),
        items: Vec::new(),
        composer: QueueObjectId::new("composer").expect("composer label"),
        prompt: QueueObjectId::new("frozen-frame").expect("frame label"),
        context_ids: vec!["msg_first".into(), "msg_second".into()],
        composer_item: None,
    }
}

fn prepared_input(separator: &str, reference_only: bool) -> PreparedComposerInput {
    let mut native = native_item(&source(), configuration()).expect("original native constituent");
    let original = companion();
    let queued = QueueLocalCapture::from_document(
        original.document,
        original.indexed.index().context_ids.clone(),
        sha256,
    )
    .expect("original delivered companion");
    let mut queued_item = queued.item().clone();
    let content = format!("{}{separator}{}", native.item.content, queued_item.content);
    let text = format!("{}{separator}{}", native.item.text, queued_item.text);
    let offset =
        native.item.text.encode_utf16().count() as u64 + separator.encode_utf16().count() as u64;
    for file in &mut queued_item.files {
        shifted(&mut file.mention, offset).expect("fixture global file anchor");
    }
    for agent in &mut queued_item.prompt_parts.agents {
        shifted(&mut agent.mention, offset).expect("fixture global agent anchor");
    }
    for skill in &mut queued_item.prompt_parts.skills {
        shifted(&mut skill.mention, offset).expect("fixture global skill anchor");
    }
    queued_item.prompt_parts.file_references = vec![QueueFileReference {
        uri: "file:///server/reference.pdf".into(),
        name: "reference.pdf".into(),
        description: Some("reference without byte ownership".into()),
        mention: Some(QueueMention {
            start: offset + 15,
            end: offset + 20,
            text: "@file".into(),
        }),
    }];
    let files = if reference_only {
        // The document owner has filtered these exact occurrences. Prefix must
        // never reload the original parts or reprocess the captured expansion.
        queued_item.files.clear();
        queued_item.prompt_parts.agents.clear();
        queued_item.prompt_parts.skills.clear();
        Vec::new()
    } else {
        original
            .retained
            .files
            .iter()
            .map(|file| file.descriptor().expect("original companion bytes"))
            .collect()
    };
    queued_item.text.clear();
    native.item.text = text.clone();
    let derived_id = MessageId::from("msg_derived_original");
    let derived = QueuePromptContext {
        id: Some(derived_id.clone()),
        text: "derived instructions".into(),
        description: None,
        metadata: None,
    };
    native.context_ids.push(derived_id);
    native.item.prompt_parts.context.push(derived.clone());
    let native_capture =
        QueueLocalCapture::new("/project", native.item, native.context_ids.clone(), sha256)
            .expect("projected native source");
    let queued_capture = QueueLocalCapture::new(
        "/project",
        queued_item,
        queued.context_ids().to_vec(),
        sha256,
    )
    .expect("globally anchored companion");
    let context_ids = native
        .context_ids
        .into_iter()
        .chain(queued.context_ids().iter().cloned())
        .collect();
    let delivery = crate::composer_delivery::ComposerDelivery {
        content,
        text,
        context_ids,
        configuration: configuration(),
        derived_contexts: vec![derived],
        sources: vec![
            crate::composer_delivery::ComposerDeliverySource {
                capture: native_capture,
                files: native.files,
                original: None,
            },
            crate::composer_delivery::ComposerDeliverySource {
                capture: queued_capture,
                files,
                original: Some(original.retained),
            },
        ],
    };
    let (capture, files) = delivery
        .into_capture()
        .expect("one exact prepared composer");
    PreparedComposerInput::new(
        capture,
        files,
        7,
        19,
        sha256(&serde_json::to_vec(&source()).expect("original source bytes")),
    )
    .expect("bounded prepared input")
}

#[test]
fn prepared_prefix_retains_exact_separators_display_delivery_context_order_and_global_anchors() {
    for separator in ["\n\n", " | λ | ", ""] {
        let input = prepared_input(separator, false);
        let plan = prepared_plan(&input);
        assert_eq!(
            plan.item.content,
            format!("👩 @build{separator}@review $skill @file display")
        );
        assert_eq!(
            plan.item.text,
            format!("👩 @build{separator}@review $skill @file delivered")
        );
        assert_eq!(
            plan.context_ids,
            [
                MessageId::from("msg_native_instruction"),
                MessageId::from("msg_native_context"),
                MessageId::from("msg_derived_original"),
                MessageId::from("msg_companion_original")
            ]
        );
        let offset = 9 + separator.encode_utf16().count() as u64;
        assert_eq!(
            plan.item.prompt_parts.agents[1]
                .mention
                .as_ref()
                .expect("global agent anchor")
                .start,
            offset
        );
        assert_eq!(
            plan.item.prompt_parts.skills[1]
                .mention
                .as_ref()
                .expect("global skill anchor")
                .start,
            offset + 8
        );
        assert_eq!(
            plan.item.files[1]
                .mention
                .as_ref()
                .expect("global byte-file anchor")
                .start,
            offset + 15
        );
        assert_eq!(
            plan.item.prompt_parts.file_references[0]
                .mention
                .as_ref()
                .expect("global reference anchor")
                .start,
            offset + 15
        );
        assert_eq!(
            plan.item
                .prompt_parts
                .metadata
                .as_ref()
                .expect("merged metadata")
                .0,
            json!({"native":true,"queued":true})
                .as_object()
                .expect("object")
                .clone()
        );
        assert_eq!(
            plan.files, input.files,
            "only owned byte files have prepared descriptors"
        );
        let bytes = input.encode().expect("exact prepared envelope");
        let restored =
            PreparedComposerInput::decode(&bytes, "/project").expect("prepared restart input");
        assert_eq!(restored.capture.bytes(), input.capture.bytes());
        assert_eq!(
            prepared_plan(&restored).item.prompt_parts,
            plan.item.prompt_parts,
            "restart must not shift anchors a second time"
        );
    }
}

#[test]
fn reference_only_prepared_prefix_keeps_the_original_expansion_without_resurrecting_exclusions() {
    let input = prepared_input("/", true);
    let plan = prepared_plan(&input);
    assert_eq!(
        plan.item.text, "👩 @build/@review $skill @file delivered",
        "reference-only edits retain the captured delivered expansion"
    );
    assert_eq!(
        plan.item.files.len(),
        1,
        "the removed companion byte file cannot return"
    );
    assert_eq!(
        plan.item.prompt_parts.agents.len(),
        1,
        "only the independent native agent remains"
    );
    assert_eq!(
        plan.item.prompt_parts.skills.len(),
        1,
        "only the independent native skill remains"
    );
    assert_eq!(plan.item.prompt_parts.file_references.len(), 1);
    assert_eq!(
        plan.item.prompt_parts.file_references[0].uri,
        "file:///server/reference.pdf"
    );
    assert_eq!(
        plan.files.len(),
        1,
        "a retained reference has no spool or byte-proof obligation"
    );
    assert_eq!(input.generation, 7);
    assert_eq!(input.revision, 19);
}

#[test]
fn prepared_prefix_freezes_reference_metadata_without_requesting_byte_custody_for_its_uri() {
    let input = prepared_input(" | ", true);
    let local = &input.capture;
    let files = input
        .files
        .iter()
        .enumerate()
        .map(|(ordinal, file)| {
            QueuePreparedFile::new_local(
                ordinal,
                file.byte_len(),
                *file.sha256(),
                &format!("file:///tmp/openchamber/0123456789abcdef/file-{ordinal}.bin"),
            )
            .expect("only prepared bytes receive a staging binding")
        })
        .collect::<Vec<_>>();
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/original/").expect("captured endpoint"),
        Credential::None,
    )
    .expect("client");
    let inputs = [QueueFrozenSource::Local(QueueLocalFrozenItem {
        capture: local,
        files: &files,
    })];
    let frozen = QueueFrozenSend::capture_local(
        &client,
        QueueLocalSendInput {
            session: &SessionId::from("ses_prefix"),
            directory: "/project",
            message_id: &MessageId::from("msg_prefix_original"),
            sources: &inputs,
            selected_input: 0,
            delivery: Some(Delivery::Steer),
        },
        sha256,
    )
    .expect("exact prepared prefix freezes");
    let body: Value = serde_json::from_slice(frozen.prompt_body()).expect("actual outgoing prompt");
    assert_eq!(body["text"], local.item().text);
    assert_eq!(
        body["files"]
            .as_array()
            .expect("prepared file and reference")
            .len(),
        2
    );
    assert_eq!(body["files"][1]["uri"], "file:///server/reference.pdf");
    assert_eq!(
        body["files"][1]["description"],
        "reference without byte ownership"
    );
    assert_eq!(
        body["files"][1]["mention"]["start"],
        local.item().prompt_parts.file_references[0]
            .mention
            .as_ref()
            .expect("global reference")
            .start
    );
    assert_eq!(frozen.context_ids(), local.context_ids());
    assert_eq!(
        files.len(),
        1,
        "the server reference cannot create a prepared-file requirement"
    );
}

#[test]
fn legacy_prefix_composition_retains_file_references_without_turning_them_into_owned_files() {
    let queued = materialized(
        json!({"format":"manifest-v1","content":"@file display","text":"@file delivered",
        "attachmentManifest":[],
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"},
        "promptParts":{"fileReferences":[{"uri":"file:///server/ref.pdf","name":"ref.pdf","description":"retained reference",
            "mention":{"start":0,"end":5,"text":"@file"}}]}}),
        &[],
        &[],
        None,
        1,
    );
    let plan = composer_plan(&source(), configuration(), vec![queued])
        .expect("older v8 reference-bearing capture");
    assert_eq!(
        plan.item.prompt_parts.file_references[0]
            .mention
            .as_ref()
            .expect("legacy reference rebased once")
            .start,
        11
    );
    assert_eq!(
        plan.item.prompt_parts.file_references[0]
            .description
            .as_deref(),
        Some("retained reference")
    );
    assert_eq!(
        plan.files.len(),
        1,
        "only the original native attachment has bytes"
    );
}

#[test]
fn rich_native_and_companion_capture_keeps_fields_original_ids_and_source_groups() {
    let source = source();
    let plan = composer_plan(&source, configuration(), vec![companion()])
        .expect("complete rich composition");
    assert_eq!(
        plan.item.content,
        "👩 @build\n\n@review $skill @file display"
    );
    assert_eq!(
        plan.item.text,
        "👩 @build\n\n@review $skill @file delivered"
    );
    assert_eq!(
        plan.context_ids,
        [
            MessageId::from("msg_native_instruction"),
            MessageId::from("msg_native_context"),
            MessageId::from("msg_companion_original")
        ]
    );
    assert_eq!(
        plan.item.prompt_parts.context[0].text,
        "native instructions"
    );
    assert_eq!(
        plan.item.prompt_parts.context[2].description.as_deref(),
        Some("original description")
    );
    assert_eq!(
        plan.item.prompt_parts.agents[1]
            .mention
            .as_ref()
            .expect("queued mention")
            .start,
        11
    );
    assert_eq!(
        plan.item.prompt_parts.skills[1]
            .mention
            .as_ref()
            .expect("skill mention")
            .start,
        19
    );
    assert_eq!(
        plan.item.files[1]
            .mention
            .as_ref()
            .expect("file mention")
            .start,
        26
    );
    assert_eq!(
        plan.item.files[1].description.as_deref(),
        Some("original file description")
    );
    assert_eq!(plan.files[0].sha256(), &sha256(b"original"));
    assert_eq!(plan.files[1].sha256(), &sha256(b"hi"));
    assert_eq!(
        source.composer.native.draft.content.files[0]
            .source_document_id
            .as_ref()
            .expect("source group")
            .as_str(),
        "source-group-original"
    );
    assert_eq!(plan.item.send_config, configuration());
    let local = QueueLocalCapture::new("/project", plan.item, plan.context_ids, sha256)
        .expect("bounded genuine local capture");
    let restored = QueueLocalCapture::from_bytes(local.bytes().to_vec(), "/project", sha256)
        .expect("same local source after restart");
    assert_eq!(restored.bytes(), local.bytes());
    assert_eq!(
        restored.item().text,
        "👩 @build\n\n@review $skill @file delivered"
    );
}

#[test]
fn legacy_companion_keeps_agent_path_bytes_and_instruction_context_order() {
    let legacy = materialized(
        json!({"content":"legacy display","text":"legacy delivered",
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"},
        "attachments":[{"id":"file-original","filename":"notes.txt","mimeType":"text/plain","size":2,
            "agentPath":"/original/source/notes.txt","dataUrl":"data:,hi"}],
        "context":[{"kind":"context","text":"legacy context","metadata":{"source":"legacy"},"instructions":"legacy instructions"}]}),
        &["msg_legacy_instruction", "msg_legacy_context"],
        &[],
        Some(&["file-original"]),
        3,
    );
    let plan = composer_plan(&source(), configuration(), vec![legacy])
        .expect("native plus legacy companion");
    assert_eq!(plan.item.files[1].name, "/original/source/notes.txt");
    assert_eq!(plan.files[1].sha256(), &sha256(b"hi"));
    assert_eq!(
        plan.context_ids[2..],
        [
            MessageId::from("msg_legacy_instruction"),
            MessageId::from("msg_legacy_context")
        ]
    );
    assert_eq!(
        plan.item.prompt_parts.context[2].text,
        "legacy instructions"
    );
    assert_eq!(plan.item.prompt_parts.context[3].text, "legacy context");
}

#[test]
fn failed_metadata_or_size_preparation_cannot_advance_to_reservation() {
    let mut source = source();
    source.composer.native.draft.content.metadata_json = Some(r#"{"queued":false}"#.into());
    let original = source.composer.native.draft.clone();
    assert!(
        composer_plan(&source, configuration(), vec![companion()]).is_err(),
        "conflicting top-level metadata refuses before any reservation"
    );
    assert_eq!(
        source.composer.native.draft, original,
        "refusal retains the whole native source"
    );
    let mut plan = native_item(&source, configuration()).expect("retained native fields");
    plan.item.prompt_parts.metadata = Some(Metadata(serde_json::Map::from_iter([(
        "oversized".into(),
        Value::String("x".repeat(crate::queue_capture::QUEUE_CAPTURE_BYTES as usize + 1)),
    )])));
    assert!(
        QueueLocalCapture::new("/project", plan.item, plan.context_ids, sha256).is_err(),
        "the complete local capture retains its 50 MiB limit"
    );
    assert!(
        QueueJson::new(
            serde_json::to_vec(
                &json!({"oversized":"x".repeat(crate::queue_capture::QUEUE_BODY_BYTES + 1)})
            )
            .expect("oversized protocol metadata")
        )
        .is_err(),
        "protocol metadata retains its separate 4 MiB limit"
    );
    assert_eq!(
        recovery_step(
            &QueuePrefixPhase::Capturing,
            Activation::Initial,
            SendObservation::Missing {
                admitted: Vec::new()
            }
        ),
        RecoveryStep::Blocked
    );
}

#[test]
fn explicit_retry_restores_original_composer_and_request_bytes_without_reencoding() {
    let plan = composer_plan(&source(), configuration(), vec![companion()])
        .expect("complete rich composer");
    let files = plan.files;
    let local = QueueLocalCapture::new("/project", plan.item, plan.context_ids, sha256)
        .expect("complete local source");
    let item = local.item();
    let manifest = QueueManifestItem {
        content: item.content.clone(),
        text: item.text.clone(),
        send_config: item.send_config.clone(),
        context: Vec::new(),
        context_preview: None,
        prompt_parts: item.prompt_parts.clone(),
        attachment_manifest: item
            .files
            .iter()
            .enumerate()
            .map(|(ordinal, file)| QueueManifestAttachment {
                attachment: QueueAttachmentDescriptor {
                    blob_id: sha256(format!("original-upload-{ordinal}").as_bytes()),
                    request_id: "original_prefix_operation".into(),
                    attachment_id: format!("composer-file-{ordinal}"),
                    filename: format!("composer-{ordinal}.bin"),
                    mime_type: file.mime_type.clone(),
                    size: file.byte_len,
                    sha256: file.sha256,
                },
                agent_path: None,
                description: file.description.clone(),
                mention: file.mention.clone(),
            })
            .collect(),
    };
    let document = QueuePayloadDocument::from_manifest(
        "/project",
        manifest,
        QueuePayloadUse::Composer,
        sha256,
    )
    .expect("real composer document");
    // Whitespace is valid captured JSON and makes any parse/encode replacement
    // observable independently of equivalent decoded fields.
    let local_bytes = format!(
        " \n{}\n",
        std::str::from_utf8(local.bytes()).expect("UTF-8 local JSON")
    )
    .into_bytes();
    let composer_bytes = format!(
        " \n{}\n",
        std::str::from_utf8(document.bytes()).expect("UTF-8 composer JSON")
    )
    .into_bytes();
    let pointer = QueuePayloadPointer::new(
        composer_bytes.len() as u64,
        sha256(&composer_bytes),
        vec![QueuePayloadChunk {
            ordinal: 0,
            attachment: QueueAttachmentDescriptor {
                blob_id: [0xef; 32],
                request_id: "original_prefix_operation".into(),
                attachment_id: "payload-0".into(),
                filename: "payload-0.json".into(),
                mime_type: "application/json".into(),
                size: composer_bytes.len() as u64,
                sha256: sha256(&composer_bytes),
            },
        }],
    )
    .expect("saved original pointer");
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/original/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let scope = QueueScope::new(&client, SessionId::from("ses_prefix"), "/project")
        .expect("captured scope");
    let capture = QueuePayloadPrefixCapture::new(
        scope.clone(),
        QueuePayloadPrefixInput {
            id: MessageId::from("msg_prefix_original"),
            item_ids: vec![QueuedItemId::new("q_first"), QueuedItemId::new("q_second")],
            composer: pointer.clone(),
            steer: true,
        },
        sha256,
    )
    .expect("strict real prefix request");
    let request_bytes = format!(
        " \n{}\n",
        std::str::from_utf8(capture.body()).expect("UTF-8 request")
    )
    .into_bytes();
    let capture = QueuePayloadPrefixCapture::from_body(scope, request_bytes.clone(), sha256)
        .expect("original request after restart");
    let (restored, document, bindings) = restore_composer_bytes(
        local_bytes.clone(),
        composer_bytes.clone(),
        "/project",
        &capture.input().composer,
        &files,
    )
    .expect("exact retry reconstruction");
    assert_eq!(capture.body(), request_bytes);
    assert_eq!(restored.bytes(), local_bytes);
    assert_eq!(document.bytes(), composer_bytes);
    assert_eq!(restored.item().files[1].name, "companion.txt");
    assert_eq!(restored.context_ids(), local.context_ids());
    assert_eq!(
        bindings
            .iter()
            .map(|binding| binding.descriptor().expect("binding"))
            .collect::<Vec<_>>(),
        files
    );
    let mut changed_files = files.clone();
    let original = changed_files[0];
    changed_files[0] = BlobDescriptor::new(*original.id(), original.byte_len(), [0; 32]);
    assert!(
        restore_composer_bytes(
            local_bytes.clone(),
            composer_bytes.clone(),
            "/project",
            &pointer,
            &changed_files
        )
        .is_err(),
        "retry refuses a changed immutable local descriptor"
    );
    let mut changed = local.item().clone();
    changed
        .prompt_parts
        .file_references
        .push(QueueFileReference {
            uri: "file:///server/unretained.pdf".into(),
            name: "unretained.pdf".into(),
            description: None,
            mention: None,
        });
    let changed = QueueLocalCapture::new("/project", changed, local.context_ids().to_vec(), sha256)
        .expect("individually valid changed source");
    assert!(
        restore_composer_bytes(
            changed.bytes().to_vec(),
            composer_bytes.clone(),
            "/project",
            &pointer,
            &files
        )
        .is_err(),
        "matching byte files cannot hide a changed reference-only prompt part"
    );
    let QueuePayloadItem::Manifest(mut named) = document.item().clone() else {
        panic!("retained typed manifest");
    };
    for (file, original) in named
        .attachment_manifest
        .iter_mut()
        .zip(&local.item().files)
    {
        file.agent_path = Some(original.name.clone());
    }
    for changed_identity in [false, true] {
        if changed_identity {
            named.attachment_manifest[0].agent_path = Some("/foreign/model/identity.txt".into());
        }
        let named_document = QueuePayloadDocument::from_manifest(
            "/project",
            named.clone(),
            QueuePayloadUse::Composer,
            sha256,
        )
        .expect("individually valid manifest identities");
        let mut chunk = pointer.chunks()[0].attachment.clone();
        chunk.size = named_document.bytes().len() as u64;
        chunk.sha256 = sha256(named_document.bytes());
        let named_pointer = named_document
            .pointer(vec![chunk], sha256)
            .expect("exact corresponding input pointer");
        let result = restore_composer_bytes(
            local_bytes.clone(),
            named_document.bytes().to_vec(),
            "/project",
            &named_pointer,
            &files,
        );
        if changed_identity {
            assert!(
                result.is_err(),
                "valid byte custody cannot replace the captured model-facing identity"
            );
        } else {
            let (restored, restored_document, _) =
                result.expect("present original identities survive explicit recovery");
            assert_eq!(restored.bytes(), local_bytes);
            assert_eq!(
                restored_document.bytes(),
                named_document.bytes(),
                "recovery retains the new field's original encoded bytes"
            );
        }
    }
    let mut chunks = pointer.chunks().to_vec();
    chunks[0].attachment.sha256 = [0; 32];
    let changed_pointer = QueuePayloadPointer::new(pointer.total_bytes(), pointer.sha256(), chunks)
        .expect("schema-valid wrong chunk digest");
    assert!(
        restore_composer_bytes(
            local_bytes,
            composer_bytes,
            "/project",
            &changed_pointer,
            &files
        )
        .is_err(),
        "the original complete chunk pointer is verified before repeat upload"
    );
}

#[test]
fn attachment_publication_requires_the_exact_original_preparation_after_the_put_wait() {
    let source = source();
    let id = AttachmentId::from("0123456789abcdef0123456789abcdef");
    let blob = BlobId::parse(id.as_str()).expect("local label");
    let descriptor = BlobDescriptor::new(blob, 2, sha256(b"hi"));
    let hold = PreparationHold {
        owner: source.composer.native.owner,
        id: id.clone(),
        generation: 1,
        blob,
    };
    for changed in 0..4 {
        let mut holds = vec![hold.clone()];
        match changed {
            0 => holds.clear(),
            1 => holds[0].generation += 1,
            2 => {
                holds[0].owner = DraftKey::new(
                    RuntimeId::from("other-host"),
                    "/project",
                    DraftTarget::Session(SessionId::from("ses_prefix")),
                )
                .expect("other owner")
            }
            _ => {
                holds[0].blob =
                    BlobId::parse("fedcba9876543210fedcba9876543210").expect("replacement label")
            }
        }
        let mut catalog = AttachmentCatalog::default();
        assert!(publish_preview_file(&mut catalog, &holds, &hold, descriptor).is_err());
        assert!(
            catalog.is_empty(),
            "a stale completion cannot create catalog authority"
        );
    }
    let mut catalog = AttachmentCatalog::default();
    assert!(
        publish_preview_file(&mut catalog, std::slice::from_ref(&hold), &hold, descriptor)
            .expect("matching durable put")
    );
    assert!(
        !publish_preview_file(&mut catalog, std::slice::from_ref(&hold), &hold, descriptor)
            .expect("same receipt is idempotent")
    );
    assert_eq!(
        catalog.get(&id).expect("published descriptor").blob,
        descriptor
    );
    let changed = BlobDescriptor::new(blob, 3, sha256(b"new"));
    assert!(
        publish_preview_file(&mut catalog, std::slice::from_ref(&hold), &hold, changed).is_err()
    );
    assert_eq!(catalog.get(&id).expect("original remains").blob, descriptor);
}

#[test]
fn lost_reservation_and_partial_context_admission_require_explicit_original_id_recovery() {
    let missing = || SendObservation::Missing {
        admitted: vec![MessageId::from("msg_first")],
    };
    for phase in [
        QueuePrefixPhase::Previewed(handoff()),
        QueuePrefixPhase::ReservationUnknown(handoff()),
    ] {
        assert_eq!(
            recovery_step(&phase, Activation::Check, missing()),
            RecoveryStep::CheckMissing,
            "Check status performs no reservation or prompt replay"
        );
        assert_eq!(
            recovery_step(&phase, Activation::Retry, missing()),
            RecoveryStep::Reserve(vec![MessageId::from("msg_first")]),
            "explicit retry repeats the saved reservation before missing context dispatch"
        );
        let pending =
            SendObservation::Pending(openchamber_native_transport::prompt::PromptReceipt {
                session_id: SessionId::from("ses_prefix"),
                message_id: MessageId::from("msg_prefix_original"),
                created: 1,
                delivery: Delivery::Steer,
            });
        assert_eq!(
            recovery_step(&phase, Activation::Check, pending),
            RecoveryStep::Admitted,
            "authoritative admission settles a lost reservation response without repeating it"
        );
    }
    assert_eq!(
        recovery_step(
            &QueuePrefixPhase::Reserved(handoff()),
            Activation::Retry,
            missing()
        ),
        RecoveryStep::Dispatch(vec![MessageId::from("msg_first")]),
        "a confirmed reservation passes the observed original leading context run to dispatch"
    );
    assert_eq!(
        recovery_step(
            &QueuePrefixPhase::Reserved(handoff()),
            Activation::Retry,
            SendObservation::OutOfOrder
        ),
        RecoveryStep::OutOfOrder
    );
}

#[test]
fn receipt_comparison_ignores_only_revision_and_rejects_changed_payload_or_identity() {
    let first = materialized(
        json!({"format":"manifest-v1","content":"first","text":"first delivered",
        "attachmentManifest":[],
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"}}),
        &[],
        &[],
        None,
        0,
    );
    let composer = materialized(
        json!({"format":"manifest-v1","content":"composer","text":"composer delivered",
        "attachmentManifest":[],
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"}}),
        &[],
        &[],
        None,
        0,
    );
    let mut preview = QueuePayloadPrefix {
        revision: 1,
        directory: "/project".into(),
        id: MessageId::from("msg_prefix_original"),
        item_ids: vec![QueuedItemId::new("q_first")],
        steer: true,
        composer: composer.indexed,
        items: vec![QueuePayloadTakenItem {
            id: QueuedItemId::new("q_first"),
            created_at: 1,
            request_id: None,
            payload: first.indexed,
        }],
        context_ids: Vec::new(),
    };
    let original =
        QueuePrefixHandoff::capture_receipt(&preview).expect("typed immutable preview receipt");
    preview.revision = 999;
    assert!(same_receipt(&original, &preview).is_ok());
    preview.items[0].id = QueuedItemId::new("q_other");
    assert!(same_receipt(&original, &preview).is_err());
    preview.items[0].id = QueuedItemId::new("q_first");
    preview.context_ids.push(MessageId::from("msg_unexpected"));
    assert!(same_receipt(&original, &preview).is_err());
    preview.context_ids.clear();
    preview.composer = materialized(
        json!({"format":"manifest-v1","content":"changed","text":"changed delivered",
        "attachmentManifest":[],
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"}}),
        &[],
        &[],
        None,
        0,
    )
    .indexed;
    assert!(
        same_receipt(&original, &preview).is_err(),
        "same item IDs cannot authorize changed composer bytes"
    );
}

#[test]
fn frozen_request_retains_local_model_paths_all_rich_fields_and_original_context_ids() {
    let plan =
        composer_plan(&source(), configuration(), vec![companion()]).expect("rich local composer");
    let mut item = plan.item;
    item.files[0].name = r#"C:\original\native.txt"#.into();
    let local = QueueLocalCapture::new("/project", item, plan.context_ids, sha256)
        .expect("model paths are metadata");
    let files = plan
        .files
        .iter()
        .enumerate()
        .map(|(ordinal, file)| {
            QueuePreparedFile::new_local(
                ordinal,
                file.byte_len(),
                *file.sha256(),
                &format!("file:///tmp/openchamber/0123456789abcdef/file-{ordinal}.bin"),
            )
            .expect("verified-byte staging binding")
        })
        .collect::<Vec<_>>();
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/original/").expect("captured original endpoint"),
        Credential::None,
    )
    .expect("client");
    let inputs = [QueueFrozenSource::Local(QueueLocalFrozenItem {
        capture: &local,
        files: &files,
    })];
    let frozen = QueueFrozenSend::capture_local(
        &client,
        QueueLocalSendInput {
            session: &SessionId::from("ses_prefix"),
            directory: "/project",
            message_id: &MessageId::from("msg_prefix_original"),
            sources: &inputs,
            selected_input: 0,
            delivery: Some(Delivery::Steer),
        },
        sha256,
    )
    .expect("all rich outgoing bytes prepared");
    let body: Value =
        serde_json::from_slice(frozen.prompt_body()).expect("actual immutable prompt JSON");
    assert_eq!(body["files"][0]["name"], r#"C:\original\native.txt"#);
    assert_eq!(body["files"][1]["description"], "original file description");
    assert_eq!(body["skills"][1]["mention"]["text"], "$skill");
    assert_eq!(body["metadata"], json!({"native":true,"queued":true}));
    assert_eq!(frozen.context_ids(), local.context_ids());
    assert_eq!(frozen.configuration(), configuration());
}

#[test]
fn captured_source_survives_new_typing_and_runtime_holds_do_not_follow_host_selection() {
    let source = source();
    let bytes = serde_json::to_vec(&source).expect("compound source");
    assert!(
        bytes.len() < 16 * 1024,
        "source contains descriptors and IDs without attachment bytes or outgoing bodies"
    );
    let restored: PrefixSourceCapture =
        serde_json::from_slice(&bytes).expect("restored original compound source");
    assert_eq!(restored.intent.id, "msg_prefix_original");
    assert_eq!(restored.intent.files[0].sha256, sha256(b"original"));
    let mut live = source.composer.native.draft.clone();
    live.edit_text("new typing after submit".into(), 23..23);
    let newer = live.clone();
    assert!(!live.consume_captured(&source.composer.native.draft));
    assert_eq!(live, newer);
    let mut state = PrefixState::default();
    let id = QueueOperationId::new("original_prefix_operation").expect("original operation");
    state.tasks.insert(
        id.clone(),
        PrefixTask {
            runtime: RuntimeId::from("original-host"),
            session: SessionId::from("ses_prefix"),
            native_roots: source
                .composer
                .native
                .draft
                .content
                .files
                .iter()
                .flat_map(|file| {
                    std::iter::once(file.id.clone()).chain(file.source_document_id.iter().cloned())
                })
                .collect(),
            _task: Task::ready(()),
        },
    );
    assert!(state.holds_runtime(&RuntimeId::from("original-host")));
    assert!(!state.holds_runtime(&RuntimeId::from("replacement-host")));
    assert!(!state.busy(&QueueOperationId::new("unrelated_operation").expect("unrelated ID")));
    assert!(
        state.references_native(&source.composer.native.draft.content.files[0].id),
        "a flush wait keeps the captured byte file alive"
    );
    assert!(
        state.references_native(
            source.composer.native.draft.content.files[0]
                .source_document_id
                .as_ref()
                .expect("original source group")
        )
    );
    state.tasks.remove(&id);
    assert!(!state.any_busy());
    assert!(
        !state.references_native(&source.composer.native.draft.content.files[0].id),
        "an unadmitted failed task releases only its ephemeral hold"
    );
}

#[test]
fn native_root_close_holds_include_foreign_runtime_prefixes_and_exclude_rootless_work() {
    let mut state = PrefixState::default();
    let rootless = QueueOperationId::new("rootless_prefix_recovery").expect("rootless operation");
    state.tasks.insert(
        rootless,
        PrefixTask {
            runtime: RuntimeId::from("active-host"),
            session: SessionId::from("ses_active"),
            native_roots: HashSet::new(),
            _task: Task::ready(()),
        },
    );
    assert!(
        state.any_busy() && !state.has_native_root_holds(),
        "rootless prefix recovery cannot delay close"
    );
    let rooted = QueueOperationId::new("captured_foreign_prefix").expect("captured operation");
    state.tasks.insert(
        rooted.clone(),
        PrefixTask {
            runtime: RuntimeId::from("original-host"),
            session: SessionId::from("ses_other"),
            native_roots: HashSet::from([AttachmentId::from("00000000000000000000000000000001")]),
            _task: Task::ready(()),
        },
    );
    assert!(
        state.has_native_root_holds(),
        "captured prefix roots block close across runtime and session switches"
    );
    state
        .tasks
        .get_mut(&rooted)
        .expect("captured actor")
        .native_roots
        .clear();
    assert!(
        state.any_busy() && !state.has_native_root_holds(),
        "ledger handoff ends the native close hold before actor completion"
    );
}

#[test]
fn payload_eligibility_keeps_rich_large_items_and_stops_at_each_real_fifo_barrier() {
    let make = |id: &str| QueuedMessage {
        id: QueuedItemId::new(id),
        content: "display".into(),
        text: "delivered".into(),
        agent_mention: None,
        attachments: vec![QueuedAttachment {
            filename: "large.bin".into(),
            source: AttachmentSource::Local,
            size: 20 * 1024 * 1024,
            server_path: None,
        }],
        has_context: true,
        context_preview: Some("context".into()),
        created_at: 1,
        send_config: Some(configuration()),
        goal: false,
        scheduled: false,
    };
    for barrier in 0..3 {
        let mut middle = make("q_barrier");
        match barrier {
            0 => middle.goal = true,
            1 => middle.scheduled = true,
            _ => middle.send_config = None,
        }
        let queue = [make("q_first"), middle, make("q_last")];
        assert_eq!(leading_ids(&queue, None), [QueuedItemId::new("q_first")]);
        assert!(leading_ids(&queue, Some(&queue[0].id)).is_empty());
    }
    assert!(
        leading_ids(&[], None).is_empty(),
        "empty queue routes ordinary sending without a reservation identity"
    );
}

#[test]
fn supersession_blocks_failed_decisions_new_arrivals_and_lost_runtime_authority() {
    use openchamber_native_core::ids::{FormId, PermissionId};
    use openchamber_native_core::request_owner::{RequestOwner, RequestScope};

    let scope = RequestScope::new(RuntimeId::from("original-host"), "/project")
        .expect("captured request scope");
    let permission = RequestKey {
        scope: scope.clone(),
        owner: RequestOwner::Session(SessionId::from("ses_prefix")),
        id: RequestId::Permission(PermissionId::from("per_original")),
    };
    let form = RequestKey {
        scope: scope.clone(),
        owner: RequestOwner::Session(SessionId::from("ses_prefix")),
        id: RequestId::Form(FormId::from("form_original")),
    };
    let captured = vec![permission, form];
    let arrival = RequestKey {
        scope,
        owner: RequestOwner::Session(SessionId::from("ses_prefix")),
        id: RequestId::Permission(PermissionId::from("per_new_arrival")),
    };
    assert!(
        !supersession_complete(&captured, &captured, true, false),
        "original pending requests require successful reject/dismiss authority"
    );
    assert!(
        !supersession_complete(&captured, &[], true, false),
        "a lost cancellation reply prevents prompt dispatch even when its card disappears"
    );
    assert!(
        !supersession_complete(&captured, std::slice::from_ref(&arrival), true, true),
        "a request arriving during preparation or cancellation blocks dispatch"
    );
    assert!(
        !captured.contains(&arrival),
        "a new arrival never joins this activation's cancellation scope"
    );
    assert!(
        supersession_complete(&captured, &[], true, true),
        "settled original requests permit the reserved frozen send"
    );
    assert!(
        !supersession_complete(&captured, &[], false, true),
        "switching hosts invalidates captured request authority"
    );
    assert!(
        !supersession_complete(&[], &[arrival], false, true),
        "new requests also block an activation that captured no decisions"
    );
    let body = serde_json::to_value(source()).expect("persisted source capture");
    assert!(
        body["intent"].get("edit").is_none(),
        "None retains the original source encoding"
    );
    assert!(
        body["intent"].get("requests").is_none(),
        "permission/form decisions never enter restart state"
    );
}

#[test]
fn native_handback_deduplicates_exact_content_and_refuses_unrestored_compound_overrides() {
    let mut captured = source();
    let mut current = captured.composer.native.draft.clone();
    current.selection = 0..0;
    assert_eq!(
        restore_abandoned_native(Some(&current), &captured, false)
            .expect("same complete native content"),
        current,
        "the captured text, attachments, contexts and mentions appear only once while the current caret is retained"
    );
    assert!(
        restore_abandoned_native(Some(&current), &captured, true).is_err(),
        "a live compound document requires its exact owner hook"
    );
    captured.intent.edit = Some(crate::queue_storage::ComposerEditRef {
        operation: QueueOperationId::new("original_edit_operation").expect("captured edit owner"),
        object: QueueObjectId::new("original-edit-object").expect("captured immutable edit"),
        generation: 1,
        revision: 2,
        byte_len: 128,
        sha256: [7; 32],
        companions: Vec::new(),
    });
    assert!(
        restore_abandoned_native(Some(&current), &captured, false).is_err(),
        "a saved override never falls back to a native-only projection"
    );
    assert_eq!(
        current.content, captured.composer.native.draft.content,
        "refusal leaves both complete inputs untouched"
    );
}

#[test]
fn only_a_complete_captured_owner_snapshot_establishes_prefix_unavailability() {
    let make = |id: &str| QueuedMessage {
        id: QueuedItemId::new(id),
        content: id.into(),
        text: id.into(),
        agent_mention: None,
        attachments: Vec::new(),
        has_context: false,
        context_preview: None,
        created_at: 1,
        send_config: Some(configuration()),
        goal: false,
        scheduled: false,
    };
    let session = SessionId::from("ses_prefix");
    let ids = vec!["q_first".into(), "q_second".into()];
    let queue = QueueSession {
        session: session.clone(),
        directory: "/project".into(),
        items: vec![make("q_first"), make("q_second"), make("q_later")],
        sending: None,
    };
    assert!(
        !observed_prefix_unavailable(&session, "/project", &ids, std::slice::from_ref(&queue))
            .expect("complete exact snapshot")
    );
    for case in 0..5 {
        let mut changed = queue.clone();
        match case {
            0 => {
                changed.items.remove(0);
            }
            1 => changed.items.swap(0, 1),
            2 => changed.sending = Some(QueuedItemId::new("q_first")),
            3 => changed.items[1].scheduled = true,
            _ => changed.directory = "/other-project".into(),
        }
        assert!(
            observed_prefix_unavailable(&session, "/project", &ids, &[changed])
                .expect("complete unavailable prefix snapshot")
        );
    }
    assert!(
        observed_prefix_unavailable(&session, "/project", &ids, &[])
            .expect("successful complete empty snapshot")
    );
    assert!(
        observed_prefix_unavailable(&session, "/project", &ids, &[queue.clone(), queue.clone()])
            .is_err(),
        "duplicate session rows grant no authority"
    );
    let mut foreign = queue;
    foreign.directory = "/spaces/012345abcdef/project".into();
    assert_ne!(
        openchamber_native_core::message_queue::queue_owner(&foreign.directory),
        openchamber_native_core::message_queue::queue_owner("/project"),
        "the control must actually name another queue owner"
    );
    assert!(
        observed_prefix_unavailable(&session, "/project", &ids, &[foreign]).is_err(),
        "another queue owner grants no absence authority"
    );
}

#[cfg(unix)]
mod durable {
    use super::*;

    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};

    use crate::attachment_io::{AttachmentFailure, AttachmentJournal};
    use crate::attachment_spool::SpoolError;
    use crate::journal_worker::{BlobFailure, CommitOutcome, JournalWorker};
    use crate::mutation_journal::{JournalCommit, JournalSnapshot, JournalView, LoadedJournal};
    use crate::queue_capture::{QueueCaptureBytes, QueueNativeSource};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Profile(PathBuf);
    impl Profile {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = PathBuf::from(format!(
                "/tmp/opencode/queue-prefix-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private test profile");
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

    fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
        JournalView::from(snapshot)
            .validate()
            .expect("complete canonical prefix snapshot");
        assert!(
            matches!(journal.save(snapshot), Ok(JournalCommit::Durable)),
            "only a durable save grants download authority"
        );
    }

    fn snapshot_copy(snapshot: &JournalSnapshot) -> JournalSnapshot {
        assert!(
            snapshot.attempts.is_empty()
                && snapshot.creations.is_empty()
                && snapshot.selections.is_empty()
                && snapshot.starter_captures.is_empty(),
            "fixture has only draft and queue owners"
        );
        JournalSnapshot {
            drafts: snapshot.drafts.clone(),
            attachments: snapshot.attachments.clone(),
            preparations: snapshot.preparations.clone(),
            retired_blobs: snapshot.retired_blobs.clone(),
            queue: snapshot.queue.clone(),
            ..JournalSnapshot::default()
        }
    }

    fn pending_download(
        journal: &mut AttachmentJournal,
        bind: bool,
    ) -> (
        JournalSnapshot,
        QueueOperationId,
        MaterializedQueueItem,
        PreparationHold,
    ) {
        pending_download_with_native(journal, bind, None)
    }

    fn pending_download_with_native(
        journal: &mut AttachmentJournal,
        bind: bool,
        captured: Option<PrefixSourceCapture>,
    ) -> (
        JournalSnapshot,
        QueueOperationId,
        MaterializedQueueItem,
        PreparationHold,
    ) {
        let original = companion();
        let owner = source().composer.native.owner;
        let native = captured
            .as_ref()
            .map(|source| source.composer.native.clone())
            .unwrap_or_else(|| QueueNativeSource {
                owner: owner.clone(),
                draft: Draft::new("native composer remains editable"),
            });
        let id =
            QueueOperationId::new("original_prefix_operation").expect("original prefix identity");
        let mut intent =
            captured
                .map(|source| source.intent)
                .unwrap_or_else(|| NativePrefixIntent {
                    kind: NativePrefixKind::Capture,
                    directory: "/project".into(),
                    id: "msg_prefix_original".into(),
                    item_ids: Vec::new(),
                    delivery: Some(Delivery::Steer),
                    companions: Vec::new(),
                    native_context_ids: Vec::new(),
                    files: Vec::new(),
                    edit: None,
                });
        intent.item_ids = vec![original.retained.item.clone()];
        let mut snapshot = JournalSnapshot::default();
        for file in &intent.files {
            let blob = BlobId::parse(&file.id).expect("captured native file label");
            let hold = PreparationHold {
                owner: owner.clone(),
                id: AttachmentId::from(file.id.clone()),
                generation: 1,
                blob,
            };
            snapshot.preparations.push(hold.clone());
            save(journal, &snapshot);
            let receipt = journal
                .put(&hold, b"original")
                .expect("original durable native bytes");
            assert_eq!(receipt.descriptor.byte_len(), file.byte_len);
            assert_eq!(receipt.descriptor.sha256(), &file.sha256);
            snapshot.attachments.insert(
                hold.id.clone(),
                PreparedAttachment {
                    blob: receipt.descriptor,
                    staging: None,
                },
            );
            snapshot.preparations.retain(|saved| saved != &hold);
        }
        let request = QueueJson::new(serde_json::to_vec(&intent).expect("internal capture intent"))
            .expect("bounded original intent");
        let operation = QueueOperation::prefix(
            id.clone(),
            QueueOwner::new(
                owner.runtime(),
                "/project",
                &SessionId::from("ses_prefix"),
                &owner,
            )
            .expect("exact owner"),
            QueueConfiguration::capture(&configuration()),
            MessageId::from("msg_prefix_original"),
            native.clone(),
            request,
        )
        .expect("capturing prefix");
        snapshot.drafts.push((native.owner, native.draft));
        assert!(
            snapshot
                .queue
                .admit_operation(operation, |_| Ok(()))
                .is_ok()
        );
        let payload = QueueCaptureBytes::new(
            QueueObjectKind::Payload,
            original.document.bytes().to_vec(),
            &[BlobId::parse("0123456789abcdef0123456789abcdef").expect("original payload label")],
        )
        .expect("original payload capture");
        let object = QueueObject::planned(
            original.retained.payload.clone(),
            id.clone(),
            QueueObjectKind::Payload,
            1,
            payload.reference().clone(),
        )
        .expect("payload hold");
        assert!(snapshot.queue.admit_object(object, |_| Ok(())).is_ok());
        save(journal, &snapshot);
        let chunk = snapshot
            .queue
            .object(&original.retained.payload)
            .expect("retained payload")
            .hold(0)
            .expect("original chunk hold");
        let receipt = journal
            .put_queue(&chunk, payload.bytes())
            .expect("committed payload put");
        snapshot
            .queue
            .record_chunk(&chunk, &receipt)
            .expect("exact durable receipt");
        save(journal, &snapshot);
        let proof = journal
            .verify_queue(
                &snapshot
                    .queue
                    .object(&original.retained.payload)
                    .expect("payload")
                    .verification(),
            )
            .expect("whole original payload verification");
        snapshot
            .queue
            .record_verified(&proof)
            .expect("retained original payload seal");
        let descriptor = original.retained.files[0]
            .descriptor()
            .expect("sealed file descriptor");
        let hold = PreparationHold {
            owner,
            id: AttachmentId::from(descriptor.id().as_str()),
            generation: 1,
            blob: *descriptor.id(),
        };
        snapshot.preparations.push(hold.clone());
        if bind {
            snapshot
                .queue
                .prepare_prefix_downloads(&id, vec![original.retained.clone()], |_| Ok(()))
                .expect("complete immutable download bindings");
        }
        save(journal, &snapshot);
        (snapshot, id, original, hold)
    }

    fn native_capture(snapshot: &JournalSnapshot, id: &QueueOperationId) -> PrefixSourceCapture {
        let QueueAction::Prefix {
            source: QueueSource::Inline(native),
            request,
            ..
        } = snapshot
            .queue
            .operation(id)
            .expect("original prefix")
            .action()
        else {
            panic!("fixture retains its complete inline native source");
        };
        let intent: NativePrefixIntent =
            serde_json::from_slice(request.bytes()).expect("original native capture intent");
        PrefixSourceCapture {
            version: 1,
            composer: CompanionCapture {
                native: native.as_ref().clone(),
                references: intent.companions.clone(),
            },
            intent,
        }
    }

    async fn open_local(
        profile: &Profile,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        cx.executor().allow_parking();
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = None;
                workspace.open_persistence(profile.journal(), window, cx);
            })
        })
        .expect("headless local storage owner");
        cx.condition(&workspace, |workspace, _| {
            workspace.persistence_ready()
                || matches!(
                    workspace.persistence_notice(),
                    Some(
                        crate::recovery_view::StorageNotice::Io
                            | crate::recovery_view::StorageNotice::Invalid
                            | crate::recovery_view::StorageNotice::Locked
                            | crate::recovery_view::StorageNotice::SyncUncertain
                            | crate::recovery_view::StorageNotice::WriteRejected
                    )
                )
        })
        .await;
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.persistence_ready(),
                "the fixture must hydrate successfully, got {:?}",
                workspace.persistence_notice()
            )
        });
        (window, workspace)
    }

    async fn finish_local(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) {
        let finish = workspace.update(cx, |workspace, _| {
            workspace.attachment_worker().expect("real worker").finish()
        });
        assert_eq!(
            finish.await.expect("worker drain"),
            Ok(CommitOutcome::Durable)
        );
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("close test-owned window");
    }

    async fn commit(workspace: &Entity<Workspace>, cx: &mut TestAppContext) {
        let (stamp, saved) = workspace.update(cx, |workspace, cx| {
            workspace
                .persistence_barrier(cx)
                .expect("complete snapshot barrier")
        });
        let result = saved.await.expect("durable save answer");
        assert!(
            workspace.update(cx, |workspace, cx| workspace
                .record_persistence(stamp, &result, cx)),
            "complete handback snapshot is durable"
        );
    }

    #[gpui_kit::test]
    async fn actual_prefix_admission_refusal_retains_removed_capture_and_independent_siblings(
        cx: &mut TestAppContext,
    ) {
        use openchamber_native_core::session::SessionRecord;

        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private prefix byte owner");
        let owner = source().composer.native.owner;
        let independent_owner = DraftKey::new(
            owner.runtime().clone(),
            "/project",
            DraftTarget::Session(SessionId::from("ses_independent_prefix")),
        )
        .expect("independent sibling owner");
        let group = AttachmentId::from("captured-prefix-source-group");
        let blobs = [0x9001, 0x9002].map(|value| {
            BlobId::parse(&format!("{value:032x}")).expect("private original file label")
        });
        let holds = blobs.map(|blob| PreparationHold {
            owner: owner.clone(),
            id: AttachmentId::from(blob.as_str()),
            generation: 1,
            blob,
        });
        let mut snapshot = JournalSnapshot::default();
        snapshot.preparations.extend_from_slice(&holds);
        save(&mut journal, &snapshot);
        let mut files = Vec::new();
        for hold in &holds {
            let receipt = journal
                .put(hold, b"original")
                .expect("actual durably held native bytes");
            snapshot.attachments.insert(
                hold.id.clone(),
                PreparedAttachment {
                    blob: receipt.descriptor,
                    staging: None,
                },
            );
            files.push(DraftFile {
                id: hold.id.clone(),
                name: Some("native.txt".into()),
                media_type: Some("text/plain".into()),
                byte_len: Some(8),
                source_document_id: Some(group.clone()),
                content: DraftFileContent::Prepared,
            });
        }
        snapshot.preparations.clear();
        let mut native = Draft::new("native prefix input");
        native.content.files = files.clone();
        snapshot.drafts.push((owner.clone(), native));
        let mut independent = Draft::new("independent sibling stays");
        independent.content.files.push(files[1].clone());
        snapshot
            .drafts
            .push((independent_owner.clone(), independent.clone()));
        let removed = files[0].id.clone();
        let shared = files[1].id.clone();
        let shared_descriptor = snapshot.attachments[&shared].blob;
        let native_files = files
            .iter()
            .map(|file| QueueNativeFileDescriptor {
                id: file.id.to_string(),
                byte_len: 8,
                sha256: sha256(b"original"),
            })
            .collect();
        save(&mut journal, &snapshot);
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        let network = crate::network::Network::new().expect("captured passive network executor");
        let client = ServerClient::new(
            Endpoint::parse("https://example.invalid/").expect("inert captured endpoint"),
            Credential::None,
        )
        .expect("captured client");
        let server = QueueServer::new(
            owner.runtime().clone(),
            client,
            network.handle(),
            SessionId::from("ses_prefix"),
            "/project",
        )
        .expect("exact original server");
        let id = QueueOperationId::new("refused_native_prefix_actor")
            .expect("original prefix operation");
        let refusal_owner = DraftKey::new(
            owner.runtime().clone(),
            "/project",
            DraftTarget::Session(SessionId::from("ses_prefix_refusal")),
        )
        .expect("unrelated exact owner");
        let refusal_bytes = cx.update_window(window, |_, window, cx| workspace.update(cx, |workspace, cx| {
            workspace.runtime = owner.runtime().clone();
            let load = workspace.sessions.begin_load();
            let _ = workspace.sessions.complete_load(load, vec![SessionRecord::new(SessionId::from("ses_prefix"), "/project")]);
            workspace.selected = Some(SessionId::from("ses_prefix"));
            workspace.composer_key = Some(owner.clone());
            workspace.restore_composer_draft(window, cx);
            let capture = workspace.capture_composer_submission(cx).expect("real native prefix capture");
            assert_eq!(capture.composer.native.draft.content.files, files);
            workspace.spawn_prefix(id.clone(), server, Activation::Initial, Some(Vec::new()), Some(InitialPrefix {
                capture, configuration: configuration(), message: MessageId::from("msg_refused_native_prefix"),
                item_ids: vec![QueuedItemId::new("queued_original")], delivery: Some(Delivery::Steer),
                native_context_ids: Vec::new(), skill_context_ids: Vec::new(), files: native_files,
            }), window, cx);
            assert!(workspace.queue.prefixes.busy(&id));
            assert!(workspace.queue.prefixes.references_native(&removed) && workspace.queue.prefixes.references_native(&group));
            assert!(workspace.queue_storage.operation(&id).is_none());
            workspace.remove_composer_attachment(&owner, &removed, window, cx);
            assert!(workspace.drafts.get(&owner).expect("live original draft").content.files.is_empty());
            assert!(workspace.attachments.contains_key(&removed) && workspace.attachment_has_owner(&removed));
            assert!(!workspace.retired_attachment_blobs.contains(&blobs[0]), "the actual actor still holds the user-removed file");
            assert!(profile.blob(&blobs[0]).is_file());
            let unrelated = QueueOperation::enqueue(id.clone(), QueueOwner::new(refusal_owner.runtime(), "/project", &SessionId::from("ses_prefix_refusal"), &refusal_owner)
                .expect("different exact queue owner"), QueueConfiguration::capture(&configuration()),
                QueueNativeSource { owner: refusal_owner.clone(), draft: Draft::new("unrelated captured original") }).expect("valid unrelated operation without file roots");
            let refusal_bytes = serde_json::to_vec(&unrelated).expect("original unrelated operation");
            workspace.change_queue_storage(|queue, workspace| queue.admit_operation(unrelated,
                |candidate| workspace.validate_queue_candidate(candidate)).map_err(|(error, _)| error)).expect("canonical same-ID admission refusal injection");
            refusal_bytes
        })).expect("the actual prefix actor captures and loses its draft owner before its first executor yield");
        cx.condition(&workspace, |workspace, _| {
            !workspace.queue.prefixes.busy(&id)
        })
        .await;
        cx.condition(&workspace, |workspace, _| {
            !workspace.attachment_retirement.busy(&blobs[0])
        })
        .await;
        workspace.read_with(cx, |workspace, cx| {
            assert!(!workspace.queue.prefixes.references_native(&removed));
            assert!(
                workspace.attachment_has_owner(&removed)
                    && workspace.queue_storage.references_captured_native(&removed)
            );
            assert!(
                workspace.attachments.contains_key(&removed),
                "registration retains the exact refused capture after its task ends"
            );
            assert!(
                workspace.composer_submissions_pending()
                    && workspace.composer_submission_failed(&owner)
            );
            assert!(
                workspace.attachments.contains_key(&shared)
                    && workspace.attachment_has_owner(&shared)
            );
            assert_eq!(workspace.drafts.get(&independent_owner), Some(&independent));
            assert!(
                workspace
                    .drafts
                    .get(&owner)
                    .expect("original input owner")
                    .content
                    .files
                    .is_empty()
            );
            assert_eq!(
                workspace.composer.read(cx).value().as_ref(),
                "native prefix input"
            );
            assert_eq!(
                serde_json::to_vec(
                    workspace
                        .queue_storage
                        .operation(&id)
                        .expect("unrelated owner remains")
                )
                .expect("same unrelated operation"),
                refusal_bytes
            );
        });
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.request_close(window, cx))
        })
        .expect("normal close refuses to lose the pending original capture");
        cx.condition(&workspace, |workspace, _| !workspace.close_in_progress())
            .await;
        assert!(workspace.read_with(cx, |workspace, _| workspace.composer_submissions_pending()));
        assert!(
            cx.update_window(window, |_, _, _| ()).is_ok(),
            "failed local admission retains the actual window"
        );
        commit(&workspace, cx).await;
        // Forced teardown exercises only the durable preexisting owners. Normal
        // close above must retain the RAM capture until its admission can fit.
        finish_local(window, &workspace, cx).await;
        let (journal, loaded) = AttachmentJournal::open(&profile.journal())
            .expect("restart after actual prefix completion");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("durable final state");
        };
        assert!(
            restored.attachments.contains_key(&removed) && profile.blob(&blobs[0]).is_file(),
            "actor cancellation grants no release authority for a still-registered capture"
        );
        assert_eq!(
            journal
                .read(&shared_descriptor)
                .expect("independent sibling remains byte-readable")
                .as_slice(),
            b"original"
        );
        assert_eq!(
            serde_json::to_vec(
                restored
                    .queue
                    .operation(&id)
                    .expect("unrelated exact owner persists")
            )
            .expect("same unrelated bytes"),
            refusal_bytes
        );
    }

    #[gpui_kit::test]
    async fn interrupted_new_prefix_keeps_its_revision_marker_and_refuses_missing_prepared_bytes(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private journal");
        let native = QueueNativeSource {
            owner: source().composer.native.owner,
            draft: Draft::new("native composer remains editable"),
        };
        let id =
            QueueOperationId::new("interrupted_marker_prefix").expect("original prefix identity");
        let intent = NativePrefixIntent {
            kind: NativePrefixKind::Capture,
            directory: "/project".into(),
            id: "msg_prefix_original".into(),
            item_ids: vec!["original_queued_item".into()],
            delivery: Some(Delivery::Steer),
            companions: Vec::new(),
            native_context_ids: Vec::new(),
            files: Vec::new(),
            edit: None,
        };
        let mut operation = QueueOperation::prefix(
            id.clone(),
            QueueOwner::new(
                native.owner.runtime(),
                "/project",
                &SessionId::from("ses_prefix"),
                &native.owner,
            )
            .expect("exact owner"),
            QueueConfiguration::capture(&configuration()),
            MessageId::from("msg_prefix_original"),
            native.clone(),
            QueueJson::new(serde_json::to_vec(&intent).expect("original intent"))
                .expect("bounded intent"),
        )
        .expect("capturing prefix");
        operation
            .begin_composer_preparation(7, 19)
            .expect("original revision marker");
        let mut snapshot = JournalSnapshot::default();
        snapshot.drafts.push((native.owner, native.draft));
        snapshot
            .queue
            .admit_operation(operation, |candidate| {
                candidate.validate(&Default::default(), &[], &[], &mut HashSet::new())
            })
            .map_err(|(error, _)| error)
            .expect("canonical initial marker admission");
        save(&mut journal, &snapshot);
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        let before = workspace.read_with(cx, |workspace, _| {
            let operation = workspace
                .queue_storage
                .operation(&id)
                .expect("original operation survives hydration");
            let marker = operation
                .composer_preparation()
                .expect("new protocol is marked in its admission snapshot");
            assert_eq!((marker.generation, marker.revision), (7, 19));
            assert!(marker.inputs_object().is_none());
            assert!(
                marker.sources.is_empty() && marker.files.is_empty(),
                "a marker grants no fabricated byte proof"
            );
            serde_json::to_vec(&workspace.queue_storage).expect("original unresolved owners")
        });
        let operation = id.clone();
        let restore = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        composer_payload::restore(&this, cx, &operation).await
                    })
                })
            })
            .expect("explicit prepared-input recovery");
        assert!(
            restore.await.is_err(),
            "restart cannot expand current snippets or invent replacement input bytes"
        );
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                serde_json::to_vec(&workspace.queue_storage).expect("retained original owners"),
                before
            );
            assert!(matches!(
                workspace
                    .queue_storage
                    .operation(&id)
                    .expect("same request")
                    .action(),
                QueueAction::Prefix {
                    phase: QueuePrefixPhase::Capturing,
                    ..
                }
            ));
        });
        finish_local(window, &workspace, cx).await;
    }

    #[gpui_kit::test]
    async fn captured_prefix_revision_cannot_consume_later_identical_native_input(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private journal");
        let (snapshot, id, _, _) = pending_download_with_native(&mut journal, true, Some(source()));
        let original = native_capture(&snapshot, &id);
        let owner = original.composer.native.owner.clone();
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        let source_bytes = serde_json::to_vec(&original).expect("original typed source");
        let operation = id.clone();
        let publish = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        queue_payload_io::persist(
                            &this,
                            cx,
                            &operation,
                            "source",
                            QueueObjectKind::NativeCompanion,
                            source_bytes,
                            true,
                        )
                        .await
                    })
                })
            })
            .expect("durable source publication");
        publish
            .await
            .expect("the original native capture has an exact published consumer");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.composer_key = Some(owner.clone());
                workspace.composer.update(cx, |composer, cx| {
                    composer.set_value(original.composer.native.draft.text.clone(), window, cx);
                    composer
                        .set_selected_range(original.composer.native.draft.selection.clone(), cx);
                });
                let captured = workspace
                    .capture_composer_submission(cx)
                    .expect("synchronous prefix submit");
                workspace.composer.update(cx, |composer, cx| {
                    composer.set_value("new independent input", window, cx)
                });
                workspace.save_composer_draft(cx);
                workspace.composer.update(cx, |composer, cx| {
                    composer.set_value(original.composer.native.draft.text.clone(), window, cx);
                    composer.set_selected_range(4..7, cx);
                });
                workspace.save_composer_draft(cx);
                assert_eq!(
                    workspace.composer.read(cx).selected_range(),
                    4..7,
                    "the fixture uses valid UTF-8 boundaries after the leading emoji"
                );
                let later = workspace
                    .drafts
                    .get(&owner)
                    .expect("later revision")
                    .clone();
                let checkpoint = workspace.composer.read(cx).history_checkpoint();
                workspace
                    .consume_composer_submission(&id, &captured, window, cx)
                    .expect("older prepared prefix consumes by revision");
                assert_eq!(
                    workspace.drafts.get(&owner),
                    Some(&later),
                    "equal text at a later revision remains independent input"
                );
                assert_eq!(
                    workspace.composer.read(cx).history_checkpoint(),
                    checkpoint,
                    "preservation must leave the current undo history intact"
                );
                assert_eq!(workspace.composer.read(cx).selected_range(), 4..7);
            })
        })
        .expect("later native editing stays available");
        finish_local(window, &workspace, cx).await;
    }

    #[test]
    fn prefix_retry_repairs_actual_partial_bytes_after_restart_under_the_same_committed_binding() {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private journal");
        let (snapshot, id, original, hold) = pending_download(&mut journal, true);
        let descriptor = original.retained.files[0]
            .descriptor()
            .expect("original descriptor");
        drop(journal);
        let mut partial = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(profile.blob(descriptor.id()))
            .expect("surviving original short write");
        partial.write_all(b"h").expect("partial original bytes");
        partial.sync_all().expect("durable partial fixture");
        drop(partial);
        let inode = fs::metadata(profile.blob(descriptor.id()))
            .expect("original inode")
            .ino();
        let (worker, loaded) = JournalWorker::start(profile.journal());
        let LoadedJournal::Present(mut restored) = loaded
            .blocking_recv()
            .expect("restart answer")
            .expect("restart snapshot")
        else {
            panic!("original prefix remains");
        };
        assert_eq!(
            fs::read(profile.blob(descriptor.id())).expect("hydration leaves bytes alone"),
            b"h"
        );
        assert!(
            restored
                .queue
                .prefix_downloads(&id)
                .is_some_and(|items| items == std::slice::from_ref(&original.retained)),
            "restart retains the exact original index, labels and byte bindings"
        );
        assert_eq!(
            worker
                .put_downloaded_attachment(hold.clone(), descriptor, b"hi".to_vec())
                .blocking_recv()
                .expect("initial put answer"),
            Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Collision
            ))),
            "Initial cannot repair an existing short file"
        );
        let mut stale = hold.clone();
        stale.generation += 1;
        assert_eq!(
            worker
                .resume_attachment(stale, descriptor, b"hi".to_vec())
                .blocking_recv()
                .expect("stale epoch answer"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        let wrong = BlobDescriptor::new(*descriptor.id(), 2, sha256(b"no"));
        assert_eq!(
            worker
                .resume_attachment(hold.clone(), wrong, b"no".to_vec())
                .blocking_recv()
                .expect("changed bytes answer"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        let mut refused = snapshot_copy(&snapshot);
        refused.drafts[0].1.content.metadata_json = Some("[]".into());
        assert!(
            worker
                .submit(refused)
                .blocking_recv()
                .expect("failed-save answer")
                .is_err()
        );
        assert_eq!(
            worker
                .resume_attachment(hold.clone(), descriptor, b"hi".to_vec())
                .blocking_recv()
                .expect("revoked authority answer"),
            Err(BlobFailure::Attachment(
                AttachmentFailure::NoDurableAuthority
            ))
        );
        assert_eq!(
            worker
                .submit(snapshot)
                .blocking_recv()
                .expect("fresh durable authority"),
            Ok(CommitOutcome::Durable)
        );
        let receipt = worker
            .resume_attachment(hold.clone(), descriptor, b"hi".to_vec())
            .blocking_recv()
            .expect("explicit retry answer")
            .expect("exact committed repair");
        assert_eq!(receipt.descriptor, descriptor);
        assert_eq!(receipt.commit, SpoolCommit::Durable);
        assert_eq!(
            fs::metadata(profile.blob(descriptor.id()))
                .expect("same file inode")
                .ino(),
            inode
        );
        assert_eq!(
            fs::read(profile.blob(descriptor.id())).expect("complete original bytes"),
            b"hi"
        );
        assert!(matches!(
            restored
                .queue
                .operation(&id)
                .expect("same request")
                .action(),
            QueueAction::Prefix {
                phase: QueuePrefixPhase::Capturing,
                ..
            }
        ));
        restored.attachments.insert(
            hold.id.clone(),
            PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        restored.preparations.retain(|saved| saved != &hold);
        assert_eq!(
            worker
                .submit(*restored)
                .blocking_recv()
                .expect("catalog publication"),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(
            worker
                .resume_attachment(hold, descriptor, b"hi".to_vec())
                .blocking_recv()
                .expect("published retry answer"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized)),
            "publication revokes unpublished repair authority"
        );
        assert_eq!(
            worker.finish().blocking_recv().expect("drain worker"),
            Ok(CommitOutcome::Durable)
        );
    }

    #[test]
    fn prefix_resume_requires_saved_bindings_and_can_resume_a_download_that_never_started() {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private journal");
        let (mut snapshot, id, original, hold) = pending_download(&mut journal, false);
        let descriptor = original.retained.files[0]
            .descriptor()
            .expect("original descriptor");
        assert_eq!(
            journal.resume_attachment(&hold, &descriptor, b"hi"),
            Err(AttachmentFailure::Unauthorized)
        );
        assert_eq!(
            journal.put_downloaded_attachment(&hold, &descriptor, b"hi"),
            Err(AttachmentFailure::Unauthorized)
        );
        snapshot
            .queue
            .prepare_prefix_downloads(&id, vec![original.retained.clone()], |_| Ok(()))
            .expect("pending in-memory bindings");
        assert_eq!(
            journal.resume_attachment(&hold, &descriptor, b"hi"),
            Err(AttachmentFailure::Unauthorized),
            "an unsaved binding grants no repair authority"
        );
        save(&mut journal, &snapshot);
        drop(journal);
        assert!(
            !profile.blob(descriptor.id()).exists(),
            "the first put never started"
        );
        let (mut journal, loaded) =
            AttachmentJournal::open(&profile.journal()).expect("restart original owner");
        assert!(matches!(loaded, LoadedJournal::Present(_)));
        assert!(
            !profile.blob(descriptor.id()).exists(),
            "hydration performs no download or write"
        );
        assert_eq!(
            journal.put_downloaded_attachment(&hold, &descriptor, b"no"),
            Err(AttachmentFailure::Spool(SpoolError::ContentMismatch)),
            "a first put verifies the original complete descriptor before creating bytes"
        );
        assert!(
            !profile.blob(descriptor.id()).exists(),
            "wrong bytes create no download file"
        );
        let receipt = journal
            .resume_attachment(&hold, &descriptor, b"hi")
            .expect("explicit retry under original binding");
        assert_eq!(receipt.descriptor, descriptor);
        assert_eq!(
            fs::read(profile.blob(descriptor.id())).expect("original bytes"),
            b"hi"
        );
    }

    #[gpui_kit::test]
    async fn capturing_abandon_restores_original_rich_native_after_new_typing_and_before_retirement(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private journal");
        let (mut snapshot, id, original, _) =
            pending_download_with_native(&mut journal, true, Some(source()));
        let captured = native_capture(&snapshot, &id);
        let owner = captured.composer.native.owner.clone();
        let native_file = captured.composer.native.draft.content.files[0].clone();
        let native_descriptor = snapshot
            .attachments
            .get(&native_file.id)
            .expect("captured native byte owner")
            .blob;
        let descriptor = original.retained.files[0]
            .descriptor()
            .expect("original descriptor");
        let mut partial = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(profile.blob(descriptor.id()))
            .expect("partial download");
        partial.write_all(b"h").expect("short original write");
        partial.sync_all().expect("durable fixture");
        drop(partial);
        let mut newer = Draft::new("newer typing remains");
        newer.selection = 1..4;
        snapshot.drafts[0].1 = newer.clone();
        save(&mut journal, &snapshot);
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        workspace.update(cx, |workspace, _| {
            workspace.runtime = RuntimeId::from("replacement-host");
            assert!(
                workspace.composer_key.is_none(),
                "handback also works without an active composer"
            );
        });
        let operation = id.clone();
        let handback = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx| {
                        abandon(&this, cx, &operation, Activation::Check).await
                    })
                })
            })
            .expect("explicit local handback");
        handback
            .await
            .expect("native handback and retirement intents are durable together");
        workspace.read_with(cx, |workspace, _| {
            let restored = workspace
                .drafts
                .get(&owner)
                .expect("original captured owner receives its source");
            assert_eq!(
                restored.text,
                format!("{}\n\n{}", newer.text, captured.composer.native.draft.text)
            );
            assert_eq!(
                restored.selection, newer.selection,
                "newer selection remains at the front"
            );
            assert_eq!(
                restored.content.files,
                captured.composer.native.draft.content.files
            );
            assert_eq!(
                restored.content.context,
                captured.composer.native.draft.content.context
            );
            assert_eq!(
                restored.content.skills,
                captured.composer.native.draft.content.skills
            );
            assert_eq!(
                restored.content.metadata_json,
                captured.composer.native.draft.content.metadata_json
            );
            assert_eq!(
                restored.content.files[0].source_document_id,
                native_file.source_document_id
            );
            assert!(
                workspace.attachments.contains_key(&native_file.id),
                "handback protects removed native attachments again"
            );
            assert!(
                !workspace
                    .retired_attachment_blobs
                    .contains(native_descriptor.id())
            );
            assert!(workspace.queue_storage.operation(&id).is_none());
        });
        finish_local(window, &workspace, cx).await;
        let (mut journal, loaded) = AttachmentJournal::open(&profile.journal())
            .expect("restart after atomic retirement intent");
        let LoadedJournal::Present(mut snapshot) = loaded else {
            panic!("saved retirement intent");
        };
        let restored = snapshot
            .drafts
            .iter()
            .find(|(key, _)| key == &owner)
            .expect("original owner survives restart");
        assert_eq!(
            restored.1.text,
            format!("{}\n\n{}", newer.text, captured.composer.native.draft.text)
        );
        assert_eq!(
            restored.1.content.files,
            captured.composer.native.draft.content.files
        );
        assert_eq!(
            restored.1.content.context,
            captured.composer.native.draft.content.context
        );
        assert_eq!(
            restored.1.content.metadata_json,
            captured.composer.native.draft.content.metadata_json
        );
        assert_eq!(
            journal
                .read(&native_descriptor)
                .expect("restored native file remains byte-readable")
                .as_slice(),
            b"original"
        );
        assert!(snapshot.queue.operation(&id).is_none());
        assert!(snapshot.retired_blobs.contains(descriptor.id()));
        assert_eq!(
            fs::read(profile.blob(descriptor.id())).expect("restart preserves partial bytes"),
            b"h"
        );
        for blob in snapshot
            .queue
            .retired_blobs()
            .expect("original prefix chunks")
        {
            let commit = journal
                .release_queue(&blob)
                .expect("explicit named chunk release");
            snapshot
                .queue
                .record_released(&blob, commit)
                .expect("durable chunk release");
        }
        assert_eq!(journal.release(descriptor.id()), Ok(SpoolCommit::Durable));
        snapshot
            .retired_blobs
            .retain(|blob| blob != descriptor.id());
        save(&mut journal, &snapshot);
    }

    #[derive(Clone, Copy)]
    enum HandbackConflict {
        Metadata,
        FileIdentity,
        RetirementCapacity,
    }

    async fn capturing_handback_refusal_restores_every_owner(
        conflict: HandbackConflict,
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new();
        let (mut journal, _) =
            AttachmentJournal::open(&profile.journal()).expect("private journal");
        let (snapshot, id, _, original_hold) =
            pending_download_with_native(&mut journal, true, Some(source()));
        let captured = native_capture(&snapshot, &id);
        let owner = captured.composer.native.owner.clone();
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        let newer = workspace.update(cx, |workspace, _| {
            let mut newer = Draft::new("newer input remains separate");
            match conflict {
                HandbackConflict::Metadata => {
                    newer.content.metadata_json = Some(r#"{"native":false}"#.into())
                }
                HandbackConflict::FileIdentity => {
                    let mut changed_file = captured.composer.native.draft.content.files[0].clone();
                    changed_file.name = Some("conflicting-immutable-name.txt".into());
                    newer.content.files.push(changed_file);
                }
                HandbackConflict::RetirementCapacity => {
                    workspace.retired_attachment_blobs = (10_000
                        ..10_000 + crate::mutation_journal::MAX_ENTRIES)
                        .map(|label| {
                            BlobId::parse(&format!("{label:032x}"))
                                .expect("independent retirement label")
                        })
                        .collect()
                }
            }
            workspace.drafts.save(owner.clone(), newer.clone());
            newer
        });
        if matches!(conflict, HandbackConflict::Metadata) {
            commit(&workspace, cx).await;
        }
        workspace.update(cx, |workspace, _| {
            let queue = serde_json::to_vec(&workspace.queue_storage)
                .expect("captured original queue owners");
            let drafts = workspace.drafts.clone();
            let catalog = workspace.attachments.clone();
            let holds = workspace.attachment_preparations.clone();
            let retired = workspace.retired_attachment_blobs.clone();
            assert!(prepare_abandon(workspace, &id, &captured, Some(&newer)).is_err());
            assert_eq!(
                serde_json::to_vec(&workspace.queue_storage).expect("unchanged retained capture"),
                queue
            );
            assert_eq!(
                workspace.drafts, drafts,
                "refusal preserves every current draft"
            );
            assert_eq!(
                workspace.attachments, catalog,
                "refusal preserves original native and download bytes"
            );
            assert_eq!(
                workspace.attachment_preparations, holds,
                "refusal cannot drop unpublished download ownership"
            );
            assert_eq!(
                workspace.retired_attachment_blobs, retired,
                "refusal creates no retirement authority"
            );
            assert!(
                workspace.queue_storage.operation(&id).is_some(),
                "explicit recovery keeps the original identity and complete source"
            );
        });
        finish_local(window, &workspace, cx).await;
        if matches!(conflict, HandbackConflict::Metadata) {
            let (_, loaded) = AttachmentJournal::open(&profile.journal())
                .expect("restart after refused metadata merge");
            let LoadedJournal::Present(snapshot) = loaded else {
                panic!("retained independent captures");
            };
            assert_eq!(
                snapshot
                    .drafts
                    .iter()
                    .find(|(key, _)| key == &owner)
                    .expect("current input survives restart")
                    .1,
                newer
            );
            assert_eq!(
                native_capture(&snapshot, &id).composer.native.draft,
                captured.composer.native.draft,
                "conflicting original text, metadata, files and source groups remain recoverable after restart"
            );
            assert_eq!(
                snapshot.preparations,
                vec![original_hold],
                "failed handback preserves the original generation and download label through restart"
            );
        }
    }

    #[gpui_kit::test]
    async fn capturing_handback_metadata_conflict_restores_every_owner(cx: &mut TestAppContext) {
        capturing_handback_refusal_restores_every_owner(HandbackConflict::Metadata, cx).await;
    }

    #[gpui_kit::test]
    async fn capturing_handback_file_identity_conflict_restores_every_owner(
        cx: &mut TestAppContext,
    ) {
        capturing_handback_refusal_restores_every_owner(HandbackConflict::FileIdentity, cx).await;
    }

    #[gpui_kit::test]
    async fn capturing_handback_late_capacity_refusal_restores_every_owner(
        cx: &mut TestAppContext,
    ) {
        capturing_handback_refusal_restores_every_owner(HandbackConflict::RetirementCapacity, cx)
            .await;
    }
}
