//! Pure envelope and canonical composer-binding contracts.

use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::draft_content::{DraftContext, DraftFile, DraftFileContent};
use openchamber_native_core::ids::{AttachmentId, ContextItemId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::prompt_parts::Metadata;
use openchamber_native_transport::queue_manifest::{
    QueueAgent, QueueFileReference, QueueLocalFile, QueueLocalItem, QueuePromptContext,
    QueuePromptParts, QueueSkill,
};

use crate::attachment_catalog::PreparedAttachment;
use crate::attachment_spool::{BlobReceipt, SpoolCommit};
use crate::draft_companion::CompanionCapture;
use crate::mutation_journal::{JournalSnapshot, JournalView};
use crate::queue_capture::{QueueNativeSource, QueueSpoolRef};
use crate::queue_storage::{
    QueueComposerPreparation, QueueConfiguration, QueueDerivedFile, QueueOwner,
    QueuePreparationMention, QueuePreparationSource, QueuePreparationSourceId,
};

use super::*;

fn owner() -> DraftKey {
    DraftKey::new(
        RuntimeId::from("payload-host"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_payload")),
    )
    .expect("captured owner")
}

fn configuration() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "selected-model".into(),
        agent: Some("build".into()),
        variant: Some("high".into()),
    }
}

fn blob(value: u128) -> BlobId {
    BlobId::parse(&format!("{value:032x}")).expect("fixed local label")
}

fn native_descriptor() -> BlobDescriptor {
    BlobDescriptor::new(blob(1), 3, sha256(b"old"))
}
fn derived_descriptor() -> BlobDescriptor {
    BlobDescriptor::new(blob(2), 3, sha256(b"new"))
}

fn local_file(descriptor: BlobDescriptor, name: &str, mime_type: &str) -> QueueLocalFile {
    QueueLocalFile {
        name: name.into(),
        mime_type: mime_type.into(),
        byte_len: descriptor.byte_len(),
        sha256: *descriptor.sha256(),
        description: Some("retained description".into()),
        mention: None,
    }
}

fn capture(files: Vec<QueueLocalFile>, contexts: &[&str]) -> QueueLocalCapture {
    let parts = QueuePromptParts {
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
            id: "retained-skill".into(),
            mention: None,
        }],
        metadata: Some(Metadata(
            serde_json::from_value(
                serde_json::json!({"marker":1,"nested":[true,{"value":"retained"}]}),
            )
            .expect("structured metadata"),
        )),
        context: contexts
            .iter()
            .map(|id| QueuePromptContext {
                id: Some(MessageId::from(*id)),
                text: format!("context for {id}"),
                description: Some("context description".into()),
                metadata: None,
            })
            .collect(),
    };
    QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: "@derived.png".into(),
            text: "@derived.png expanded".into(),
            files,
            send_config: configuration(),
            prompt_parts: parts,
        },
        contexts.iter().map(|id| MessageId::from(*id)).collect(),
        sha256,
    )
    .expect("already projected local capture")
}

fn basic_input() -> PreparedComposerInput {
    PreparedComposerInput::new(
        capture(
            vec![local_file(native_descriptor(), "native.txt", "text/plain")],
            &["msg_native"],
        ),
        vec![native_descriptor()],
        9,
        4,
        sha256(b"captured original source"),
    )
    .expect("complete input")
}

fn metadata_and_capture(encoded: &[u8]) -> (serde_json::Value, &[u8]) {
    let length = u32::from_be_bytes(encoded[8..12].try_into().expect("metadata length")) as usize;
    (
        serde_json::from_slice(&encoded[12..12 + length]).expect("metadata JSON"),
        &encoded[12 + length..],
    )
}

fn rewrite_metadata(encoded: &[u8], change: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let (mut metadata, capture) = metadata_and_capture(encoded);
    change(&mut metadata);
    let metadata = serde_json::to_vec(&metadata).expect("modified test metadata");
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&metadata);
    bytes.extend_from_slice(capture);
    bytes
}

#[test]
fn version_one_round_trip_preserves_original_capture_bytes_and_rich_fields() {
    let mut input = basic_input();
    let exact = std::str::from_utf8(input.capture.bytes())
        .expect("capture JSON")
        .replace("\"marker\":1", "\"marker\":1.000")
        .into_bytes();
    input.capture = QueueLocalCapture::from_bytes(exact.clone(), "/project", sha256)
        .expect("original numeric spelling");
    let encoded = input.encode().expect("binary envelope");
    assert_eq!(
        &encoded[..8],
        b"OCCPIN01",
        "the envelope has its own stable versioned magic"
    );
    let (metadata, original) = metadata_and_capture(&encoded);
    assert_eq!(metadata["version"], 1);
    assert_eq!(
        original, exact,
        "the original capture is appended without reserialization"
    );
    assert!(
        metadata.get("capture").is_none(),
        "capture bytes do not become a JSON or base64 copy"
    );
    let restored =
        PreparedComposerInput::decode(&encoded, "/project").expect("bounded input decode");
    assert_eq!(restored.capture.bytes(), input.capture.bytes());
    assert_eq!(restored.encode().expect("stable re-encoding"), encoded);
    assert_eq!(restored.files, input.files);
    assert_eq!(restored.generation, 9);
    assert_eq!(restored.revision, 4);
    assert_eq!(restored.source_sha256, input.source_sha256);
    assert_eq!(
        restored.capture.item().prompt_parts.file_references[0].uri,
        "file:///project/reference.pdf"
    );
    assert_eq!(
        restored.capture.item().prompt_parts.skills[0].id,
        "retained-skill"
    );
    assert_eq!(
        restored.capture.item().files[0].description.as_deref(),
        Some("retained description")
    );
    assert_eq!(format!("{restored:?}"), "PreparedComposerInput([redacted])");
}

#[test]
fn malformed_future_truncated_and_wrong_directory_envelopes_are_rejected() {
    let encoded = basic_input().encode().expect("valid envelope");
    for length in [0, 1, 7, 8, 11, encoded.len() - 1] {
        assert!(PreparedComposerInput::decode(&encoded[..length], "/project").is_err());
    }
    let mut wrong_magic = encoded.clone();
    wrong_magic[..8].copy_from_slice(b"OTHER001");
    assert!(
        PreparedComposerInput::decode(&wrong_magic, "/project").is_err(),
        "other private input formats do not fall through"
    );
    assert!(PreparedComposerInput::decode(&encoded, "/other").is_err());
    let mut trailing = encoded.clone();
    trailing.push(b' ');
    assert!(
        PreparedComposerInput::decode(&trailing, "/project").is_err(),
        "capture length binds the complete envelope"
    );
    for change in 0..8 {
        let changed = rewrite_metadata(&encoded, |metadata| match change {
            0 => metadata["version"] = serde_json::json!(2),
            1 => metadata["generation"] = serde_json::json!(0),
            2 => metadata["revision"] = serde_json::json!(u64::MAX),
            3 => metadata["capture_len"] = serde_json::json!(u64::MAX),
            4 => metadata["capture_sha256"] = serde_json::json!(vec![0u8; 32]),
            5 => metadata["unknown"] = serde_json::json!(true),
            6 => metadata["files"][0]["id"] = serde_json::json!("not-a-local-label"),
            _ => {
                metadata
                    .as_object_mut()
                    .expect("metadata object")
                    .remove("source_sha256");
            }
        });
        assert!(
            PreparedComposerInput::decode(&changed, "/project").is_err(),
            "malformed metadata is never an empty capture"
        );
    }
    let mut corrupt = encoded.clone();
    let length = u32::from_be_bytes(corrupt[8..12].try_into().expect("metadata length")) as usize;
    let first = 12 + length;
    corrupt[first] = b'[';
    assert!(PreparedComposerInput::decode(&corrupt, "/project").is_err());
    let mut oversized_header = encoded;
    oversized_header[8..12].copy_from_slice(&((QUEUE_BODY_BYTES + 1) as u32).to_be_bytes());
    assert!(PreparedComposerInput::decode(&oversized_header, "/project").is_err());
    let encoded = basic_input().encode().expect("valid envelope");
    let too_many_files = rewrite_metadata(&encoded, |metadata| {
        metadata["files"] = serde_json::json!(vec![metadata["files"][0].clone(); MAX_ENTRIES + 1]);
    });
    assert!(PreparedComposerInput::decode(&too_many_files, "/project").is_err());
}

#[test]
fn envelope_limits_include_metadata_and_the_full_original_capture() {
    let limit = usize::try_from(QUEUE_CAPTURE_BYTES).expect("native byte limit");
    let capture_len = limit - PREFIX_BYTES - QUEUE_BODY_BYTES;
    assert_eq!(
        envelope_len(QUEUE_BODY_BYTES, capture_len).expect("exact complete-object bound"),
        limit
    );
    assert!(envelope_len(QUEUE_BODY_BYTES, capture_len + 1).is_err());
    assert!(envelope_len(QUEUE_BODY_BYTES + 1, 1).is_err());
    assert!(envelope_len(0, 1).is_err());
    assert!(envelope_len(1, 0).is_err());
    assert!(envelope_len(usize::MAX, 1).is_err());
    let mut input = basic_input();
    input.files = vec![native_descriptor(); MAX_ENTRIES + 1];
    assert!(
        input.encode().is_err(),
        "public field changes cannot bypass the file-count limit"
    );
    for (generation, revision) in [(0, 1), (u64::MAX, 1), (1, 0), (1, u64::MAX)] {
        assert!(
            PreparedComposerInput::new(
                capture(Vec::new(), &[]),
                Vec::new(),
                generation,
                revision,
                [1; 32]
            )
            .is_err()
        );
    }
}

#[test]
fn occurrence_order_is_exact_and_equal_descriptor_repeats_remain_valid() {
    let descriptor = native_descriptor();
    let files = vec![
        local_file(descriptor, "first.txt", "text/plain"),
        local_file(descriptor, "second.txt", "text/plain"),
    ];
    let repeated = PreparedComposerInput::new(
        capture(files, &[]),
        vec![descriptor, descriptor],
        1,
        1,
        [1; 32],
    )
    .expect("the same held bytes can occur twice");
    let restored = PreparedComposerInput::decode(
        &repeated.encode().expect("repeated descriptors"),
        "/project",
    )
    .expect("repeated occurrences survive restore");
    assert_eq!(restored.capture.item().files[0].name, "first.txt");
    assert_eq!(restored.capture.item().files[1].name, "second.txt");
    let conflicting = BlobDescriptor::new(*descriptor.id(), 4, sha256(b"else"));
    let files = vec![
        local_file(descriptor, "first.txt", "text/plain"),
        local_file(conflicting, "second.txt", "text/plain"),
    ];
    assert!(
        PreparedComposerInput::new(
            capture(files, &[]),
            vec![descriptor, conflicting],
            1,
            1,
            [1; 32]
        )
        .is_err(),
        "one local identity cannot describe two byte streams"
    );
    let files = vec![
        local_file(descriptor, "native.txt", "text/plain"),
        local_file(derived_descriptor(), "derived.png", "image/png"),
    ];
    assert!(
        PreparedComposerInput::new(
            capture(files, &[]),
            vec![derived_descriptor(), descriptor],
            1,
            1,
            [1; 32]
        )
        .is_err(),
        "file occurrence descriptors cannot be reordered independently"
    );
}

struct Fixture {
    snapshot: JournalSnapshot,
    id: QueueOperationId,
    input: PreparedComposerInput,
}

fn fixture() -> Fixture {
    let mut draft = Draft::new("@derived.png");
    draft.content.files.push(DraftFile {
        id: AttachmentId::from(blob(1).as_str()),
        name: Some("native.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(3),
        source_document_id: None,
        content: DraftFileContent::Prepared,
    });
    draft.content.context.push(DraftContext {
        id: ContextItemId::from("native-context"),
        text: "original context".into(),
        description: None,
        instructions: None,
        metadata_json: None,
    });
    let operation = QueueOperation::local_send(
        QueueOperationId::new("prepared_payload").expect("operation label"),
        QueueOwner::new(
            owner().runtime(),
            "/project",
            &SessionId::from("ses_payload"),
            &owner(),
        )
        .expect("owner"),
        QueueConfiguration::capture(&configuration()),
        MessageId::from("msg_payload_prompt"),
        CompanionCapture {
            native: QueueNativeSource {
                owner: owner(),
                draft,
            },
            references: Vec::new(),
        },
        None,
        vec!["msg_native".into()],
        vec![QueueNativeFileDescriptor {
            id: blob(1).as_str().into(),
            byte_len: 3,
            sha256: sha256(b"old"),
        }],
    )
    .expect("captured original operation");
    let id = operation.id().clone();
    let source_sha256 = *operation.source_sha256().expect("original source seal");
    let original = capture(
        vec![local_file(native_descriptor(), "native.txt", "text/plain")],
        &["msg_native"],
    );
    let derived = QueueDerivedFile {
        source: QueuePreparationSourceId::Source(0),
        mention: QueuePreparationMention {
            start: 0,
            end: 12,
            text: "@derived.png".into(),
        },
        server_path: "/project/derived.png".into(),
        output_ordinal: 0,
        local_attachment_id: blob(2).as_str().into(),
        blob: blob(2).as_str().into(),
        preparation_generation: 9,
        raw_source_byte_len: 3,
        raw_source_sha256: sha256(b"new"),
        byte_len: 3,
        sha256: sha256(b"new"),
        name: "derived.png".into(),
        mime_type: "image/png".into(),
        document_group: None,
        state: QueueDerivedFileState::Proposed,
    };
    let plan = QueueComposerPreparation::new(
        &owner(),
        source_sha256,
        9,
        4,
        vec![QueuePreparationSource {
            identity: QueuePreparationSourceId::Source(0),
            byte_len: original.bytes().len() as u64,
            sha256: original.sha256(),
        }],
        vec![derived],
        vec!["msg_derived".into()],
    );
    let mut snapshot = JournalSnapshot::default();
    snapshot.attachments.insert(
        AttachmentId::from(blob(1).as_str()),
        PreparedAttachment {
            blob: native_descriptor(),
            staging: None,
        },
    );
    snapshot
        .queue
        .admit_operation(operation, |_| Ok(()))
        .map_err(|(error, _)| error)
        .expect("original owner admitted");
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            plan,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, preparation| {
                if preparation.sources[0].byte_len != original.bytes().len() as u64
                    || preparation.sources[0].sha256 != original.sha256()
                    || preparation.files[0].mention.text != original.item().content
                {
                    return Err(JournalError::Invalid(
                        "captured source or explicit mention changed",
                    ));
                }
                Ok(())
            },
            |_, _| Ok(()),
        )
        .expect("exact preparation plan");
    snapshot
        .queue
        .publish_composer_derived_file(
            &id,
            9,
            0,
            &BlobReceipt {
                descriptor: derived_descriptor(),
                commit: SpoolCommit::Durable,
            },
            &mut snapshot.attachments,
            &mut snapshot.preparations,
            |_, _, _| Ok(()),
        )
        .expect("pure exact-receipt transition");
    JournalView::from(&snapshot)
        .validate()
        .expect("canonical fixture with all retained owners");
    let prepared = capture(
        vec![
            local_file(native_descriptor(), "native.txt", "text/plain"),
            local_file(derived_descriptor(), "derived.png", "image/png"),
        ],
        &["msg_native", "msg_derived"],
    );
    let input = PreparedComposerInput::new(
        prepared,
        vec![native_descriptor(), derived_descriptor()],
        9,
        4,
        source_sha256,
    )
    .expect("complete canonical prepared input");
    Fixture {
        snapshot,
        id,
        input,
    }
}

fn replace_item(input: &mut PreparedComposerInput, change: impl FnOnce(&mut QueueLocalItem)) {
    let mut item = input.capture.item().clone();
    change(&mut item);
    let ids = item
        .prompt_parts
        .context
        .iter()
        .map(|context| context.id.clone().expect("stable context identity"))
        .collect();
    input.capture = QueueLocalCapture::new("/project", item, ids, sha256)
        .expect("well-formed modified local capture");
}

#[test]
fn validation_binds_original_seal_generation_revision_configuration_and_full_context_order() {
    let fixture = fixture();
    fixture
        .input
        .validate(&fixture.snapshot.queue, &fixture.id)
        .expect("canonical binding");
    for mutation in 0..6 {
        let mut input = PreparedComposerInput::decode(
            &fixture.input.encode().expect("original bytes"),
            "/project",
        )
        .expect("input");
        match mutation {
            0 => input.source_sha256 = sha256(b"another original source"),
            1 => input.generation += 1,
            2 => input.revision += 1,
            3 => replace_item(&mut input, |item| {
                item.send_config.model_id = "another-model".into()
            }),
            4 => replace_item(&mut input, |item| item.prompt_parts.context.swap(0, 1)),
            _ => {
                input.capture = QueueLocalCapture::new(
                    "/other",
                    input.capture.item().clone(),
                    input.capture.context_ids().to_vec(),
                    sha256,
                )
                .expect("different valid capture directory");
            }
        }
        assert!(
            input
                .validate(&fixture.snapshot.queue, &fixture.id)
                .is_err(),
            "a valid envelope does not grant another plan's authority"
        );
    }
    let absent = QueueOperationId::new("missing_owner").expect("other identity");
    assert!(
        fixture
            .input
            .validate(&fixture.snapshot.queue, &absent)
            .is_err()
    );
}

#[test]
fn full_canonical_context_order_includes_companions_after_native_and_derived_contexts() {
    let mut fixture = fixture();
    replace_item(&mut fixture.input, |item| {
        item.prompt_parts.context.push(QueuePromptContext {
            id: Some(MessageId::from("msg_companion")),
            text: "original companion context".into(),
            description: None,
            metadata: None,
        })
    });
    let operation = fixture
        .snapshot
        .queue
        .operation(&fixture.id)
        .expect("operation");
    let roots = fixture
        .snapshot
        .queue
        .composer_file_descriptors(&fixture.id)
        .expect("canonical byte roots");
    let full_order = [
        "msg_native".to_owned(),
        "msg_derived".to_owned(),
        "msg_companion".to_owned(),
    ];
    fixture
        .input
        .validate_operation(operation, &full_order, &roots)
        .expect("full canonical dependency order");
    replace_item(&mut fixture.input, |item| {
        item.prompt_parts.context.swap(1, 2)
    });
    assert!(
        fixture
            .input
            .validate_operation(operation, &full_order, &roots)
            .is_err(),
        "retained companion identities cannot move before the derived native instructions"
    );
}

#[test]
fn validation_rejects_unrooted_changed_missing_and_uncataloged_derived_outputs() {
    let fixture = fixture();
    for mutation in 0..4 {
        let mut input = PreparedComposerInput::decode(
            &fixture.input.encode().expect("original bytes"),
            "/project",
        )
        .expect("input");
        match mutation {
            0 => input.files[0] = BlobDescriptor::new(blob(99), 3, sha256(b"old")),
            1 => replace_item(&mut input, |item| {
                item.files[1].name = "replacement.png".into()
            }),
            2 => replace_item(&mut input, |item| {
                item.files[1].mime_type = "text/plain".into()
            }),
            _ => {
                input.files.pop();
                replace_item(&mut input, |item| {
                    item.files.pop();
                });
            }
        }
        assert!(
            input
                .validate(&fixture.snapshot.queue, &fixture.id)
                .is_err()
        );
    }
    let mut unready =
        serde_json::to_value(&fixture.snapshot.queue).expect("pure persisted ledger fixture");
    unready["operations"][0]["preparation"]["files"][0]["state"] = serde_json::json!("proposed");
    let unready: QueueStorage =
        serde_json::from_value(unready).expect("uncataloged proposal shape");
    assert!(
        fixture.input.validate(&unready, &fixture.id).is_err(),
        "no derived proposal may enter a sealed payload"
    );
    let mut missing_plan = serde_json::to_value(&fixture.snapshot.queue).expect("ledger");
    missing_plan["operations"][0]["preparation"] = serde_json::Value::Null;
    let missing_plan: QueueStorage =
        serde_json::from_value(missing_plan).expect("older v8 without preparation");
    assert!(
        fixture.input.validate(&missing_plan, &fixture.id).is_err(),
        "legacy recovery never acquires new prepared-input authority implicitly"
    );
}

#[test]
fn canonical_validation_allows_repeated_equal_occurrences_and_preserves_reference_only_parts() {
    let mut fixture = fixture();
    fixture.input.files.push(native_descriptor());
    replace_item(&mut fixture.input, |item| {
        item.files.push(item.files[0].clone())
    });
    fixture
        .input
        .validate(&fixture.snapshot.queue, &fixture.id)
        .expect("same rooted descriptor repeated in occurrence order");
    assert_eq!(
        fixture
            .input
            .capture
            .item()
            .prompt_parts
            .file_references
            .len(),
        1
    );
    assert_eq!(
        fixture.input.capture.item().prompt_parts.file_references[0].uri,
        "file:///project/reference.pdf"
    );
    assert_eq!(
        fixture.input.files.len(),
        3,
        "a path-only PDF reference is not an inline-byte occurrence"
    );
}

#[test]
fn verified_object_binding_checks_owner_kind_generation_complete_length_and_digest() {
    let fixture = fixture();
    let operation = fixture
        .snapshot
        .queue
        .operation(&fixture.id)
        .expect("original owner");
    let bytes = fixture.input.encode().expect("exact binary input bytes");
    let digest = sha256(&bytes);
    let id = QueueObjectId::new("prepared_payload-inputs").expect("stable inputs role");
    let descriptor = BlobDescriptor::new(blob(40), bytes.len() as u64, digest);
    let reference = QueueSpoolRef::new(
        bytes.len() as u64,
        digest,
        vec![descriptor],
        QueueObjectKind::NativeCompanion,
    )
    .expect("small exact object");
    let object = QueueObject::planned(
        id.clone(),
        fixture.id.clone(),
        QueueObjectKind::NativeCompanion,
        9,
        reference,
    )
    .expect("planned object metadata");
    assert!(
        validate_object(
            &fixture.input,
            operation,
            &object,
            &id,
            bytes.len() as u64,
            &digest
        )
        .is_err(),
        "planned metadata alone supplies no verified-byte binding"
    );
    let mut value = serde_json::to_value(&object).expect("pure verified-object metadata fixture");
    value["progress"] = serde_json::json!("verified");
    let verified: QueueObject =
        serde_json::from_value(value.clone()).expect("verified metadata shape");
    validate_object(
        &fixture.input,
        operation,
        &verified,
        &id,
        bytes.len() as u64,
        &digest,
    )
    .expect("matching verified object");
    assert!(
        validate_object(
            &fixture.input,
            operation,
            &verified,
            &id,
            bytes.len() as u64 + 1,
            &digest
        )
        .is_err()
    );
    assert!(
        validate_object(
            &fixture.input,
            operation,
            &verified,
            &id,
            bytes.len() as u64,
            &[0; 32]
        )
        .is_err()
    );
    for mutation in 0..3 {
        let mut changed = value.clone();
        match mutation {
            0 => changed["generation"] = serde_json::json!(10),
            1 => changed["operation"] = serde_json::json!("other_operation"),
            _ => changed["kind"] = serde_json::json!("payload"),
        }
        let changed: QueueObject =
            serde_json::from_value(changed).expect("different object metadata");
        assert!(
            validate_object(
                &fixture.input,
                operation,
                &changed,
                &id,
                bytes.len() as u64,
                &digest
            )
            .is_err()
        );
    }
}
