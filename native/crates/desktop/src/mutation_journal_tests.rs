use super::*;
use crate::attachment_catalog::{PreparedAttachment, StagingDescriptor, StagingPhase};
use crate::attachment_spool::BlobDescriptor;
use crate::send_attempt::{SendAttempts, tests as attempt_tests};
use openchamber_native_core::draft_content::{
    DraftContent, DraftContext, DraftFile, DraftFileContent,
};
use openchamber_native_core::ids::ContextItemId;

fn round_trip(snapshot: &JournalSnapshot) -> JournalSnapshot {
    let bytes = serde_json::to_vec(&Store::capture(snapshot).expect("valid snapshot"))
        .expect("encode snapshot");
    decode_store(&bytes)
        .expect("decode snapshot")
        .into_snapshot()
        .expect("restore snapshot")
}

fn value(snapshot: &JournalSnapshot) -> serde_json::Value {
    serde_json::to_value(Store::capture(snapshot).expect("valid snapshot")).expect("JSON")
}

fn restore_value(value: &serde_json::Value) -> Result<JournalSnapshot, JournalError> {
    decode_store(&serde_json::to_vec(value).expect("JSON"))?.into_snapshot()
}

fn prepared(id: u128) -> (DraftFile, PreparedAttachment) {
    let id = format!("{id:032x}");
    let blob_id = attachment_catalog::blob_id(&id).expect("blob identity");
    let file = DraftFile {
        id: AttachmentId::from(id),
        name: Some("notes.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(5),
        source_document_id: None,
        content: DraftFileContent::Prepared,
    };
    (
        file,
        PreparedAttachment {
            blob: BlobDescriptor::new(blob_id, 5, [0x5a; 32]),
            staging: None,
        },
    )
}

fn content_sample() -> JournalSnapshot {
    let mut snapshot = sample();
    let draft = &mut snapshot.drafts[0].1;
    draft.text = "@plan é👩".into();
    draft.selection = 6..8;
    draft.content = DraftContent {
        files: vec![DraftFile {
            id: AttachmentId::from("reference-id"),
            name: Some("notes ü.txt".into()),
            media_type: None,
            byte_len: None,
            source_document_id: Some(AttachmentId::from("source-document")),
            content: DraftFileContent::Reference {
                uri: "data:text/plain;base64,AAEK/w==".into(),
            },
        }],
        agents: vec![AgentMention {
            name: "plan".into(),
            mention: Some(Mention {
                start: 0,
                end: 5,
                text: "@plan".into(),
            }),
        }],
        skills: vec![SkillRef {
            id: "skill_1".into(),
        }],
        metadata_json: Some("{ \"exact\" : [1, true], \"unicode\":\"λ\" }".into()),
        context: vec![DraftContext {
            id: ContextItemId::from("quote-1"),
            text: "quoted\0text\r\nλ".into(),
            description: Some("Selection".into()),
            metadata_json: Some("{\"kind\":\"quote\"}".into()),
            instructions: Some("Use this exact context.\n".into()),
        }],
    };
    let (file, attachment) = prepared(1);
    draft.content.files.push(file.clone());
    snapshot.attachments.insert(file.id, attachment);
    snapshot
}

fn starter_sample() -> JournalSnapshot {
    let mut snapshot = content_sample();
    let (owner, draft) = snapshot.drafts.remove(0);
    let prepared = &draft.content.files[1];
    let uri = "file:///tmp/opencode/openchamber/0123456789abcdef/notes.txt";
    snapshot
        .attachments
        .get_mut(&prepared.id)
        .expect("prepared source")
        .staging = Some(StagingDescriptor {
        runtime: owner.runtime().clone(),
        directory: owner.directory().to_owned(),
        temporary_directory: "/tmp/opencode".into(),
        server_path: "/tmp/opencode/openchamber/0123456789abcdef/notes.txt".into(),
        uri: uri.into(),
        filename: "notes.txt".into(),
        phase: StagingPhase::Ready,
    });
    let mut parts = PromptParts {
        agents: draft.content.agents.clone(),
        skills: draft.content.skills.clone(),
        metadata: draft
            .content
            .metadata_json
            .as_deref()
            .map(|json| Metadata(serde_json::from_str(json).expect("source metadata object"))),
        files: vec![
            PromptFile {
                uri: match &draft.content.files[0].content {
                    DraftFileContent::Reference { uri } => uri.clone(),
                    DraftFileContent::Prepared => panic!("reference fixture"),
                },
                name: draft.content.files[0].name.clone(),
            },
            PromptFile {
                uri: uri.into(),
                name: prepared.name.clone(),
            },
        ],
        ..PromptParts::default()
    };
    let source = &draft.content.context[0];
    parts.context = vec![
        SyntheticContext {
            id: MessageId::from("msg_starter_source_instructions"),
            text: source.instructions.clone().expect("source instructions"),
            description: None,
            metadata: None,
        },
        SyntheticContext {
            id: MessageId::from("msg_starter_source_context"),
            text: source.text.clone(),
            description: source.description.clone(),
            metadata: source
                .metadata_json
                .as_deref()
                .map(|json| Metadata(serde_json::from_str(json).expect("context metadata object"))),
        },
    ];
    snapshot.starter_captures.push(StarterCapture {
        id: MessageId::from("msg_starter_capture"),
        owner,
        project: "registered-project".into(),
        preset_text: format!("/explore {}", draft.text),
        draft,
        parts,
        selection: Selection {
            agent_name: Some("plan".into()),
            model: Some(ModelChoice {
                provider_id: "openchamber".into(),
                model_id: "auto".into(),
                variant: Some("high".into()),
            }),
        },
        permission_mode: Some(PermissionMode::Safety),
    });
    snapshot
}

#[test]
fn v7_starter_captures_round_trip_full_source_configuration_parts_and_permissions() {
    for permission_mode in [
        None,
        Some(PermissionMode::Ask),
        Some(PermissionMode::Safety),
        Some(PermissionMode::Auto),
    ] {
        let mut snapshot = starter_sample();
        snapshot.starter_captures[0].permission_mode = permission_mode;
        let restored = round_trip(&snapshot);
        assert_eq!(restored.starter_captures, snapshot.starter_captures);
        assert_eq!(restored.attachments, snapshot.attachments);
        assert!(
            restored.drafts.is_empty(),
            "the capture owns the source independently of later typing"
        );
    }
    let mut empty_source = starter_sample();
    empty_source.starter_captures[0].draft = Draft::default();
    empty_source.starter_captures[0].parts = PromptParts::default();
    empty_source.starter_captures[0].preset_text = "/explore".into();
    assert_eq!(
        round_trip(&empty_source).starter_captures,
        empty_source.starter_captures,
        "a starter needs no pre-existing composer text or parts"
    );
}

#[test]
fn v7_starter_schema_requires_all_fields_and_rejects_unknown_or_inconsistent_content() {
    let original = value(&starter_sample());
    for (parent, field) in [
        ("", "starter_captures"),
        ("/starter_captures/0", "draft"),
        ("/starter_captures/0", "project"),
        ("/starter_captures/0", "preset_text"),
        ("/starter_captures/0", "prompt"),
        ("/starter_captures/0", "permission_mode"),
        ("/starter_captures/0/draft", "content"),
        ("/starter_captures/0/prompt", "message_id"),
        ("/starter_captures/0/prompt", "text"),
        ("/starter_captures/0/prompt", "model"),
        ("/starter_captures/0/prompt", "agent"),
        ("/starter_captures/0/prompt", "delivery"),
        ("/starter_captures/0/prompt", "metadata"),
        ("/starter_captures/0/prompt/model", "variant"),
        ("/starter_captures/0/prompt/files/0", "name"),
        ("/starter_captures/0/prompt/context/0", "description"),
        ("/starter_captures/0/prompt/context/0", "metadata"),
    ] {
        let mut missing = original.clone();
        missing
            .pointer_mut(parent)
            .expect("parent")
            .as_object_mut()
            .expect("object")
            .remove(field)
            .expect("required field");
        assert!(
            restore_value(&missing).is_err(),
            "reject missing {parent}/{field}"
        );
    }
    for (path, replacement) in [
        (
            "/starter_captures/0/permission_mode",
            serde_json::json!("future"),
        ),
        (
            "/starter_captures/0/prompt/text",
            serde_json::json!("different rendered text"),
        ),
        (
            "/starter_captures/0/prompt/delivery",
            serde_json::json!("steer"),
        ),
        ("/starter_captures/0/prompt/model", serde_json::Value::Null),
        ("/starter_captures/0/prompt/metadata", serde_json::json!([])),
        (
            "/starter_captures/0/draft/content",
            serde_json::json!("captured_prompt"),
        ),
        (
            "/starter_captures/0/draft/content/owned/metadata_json",
            serde_json::json!("[]"),
        ),
    ] {
        let mut malformed = original.clone();
        *malformed.pointer_mut(path).expect("field") = replacement;
        assert!(
            restore_value(&malformed).is_err(),
            "reject inconsistent {path}"
        );
    }
    let mut unknown = original;
    unknown["starter_captures"][0]["unknown"] = true.into();
    assert!(restore_value(&unknown).is_err());
}

#[test]
fn starter_capture_conflict_keeps_the_source_attachment_root_and_original_owner() {
    use crate::starter_capture::StarterCaptures;
    use openchamber_native_core::draft::{ContentConflict, DraftStore};

    let mut snapshot = starter_sample();
    let restored = round_trip(&snapshot);
    let original = restored.starter_captures[0].clone();
    let mut drafts = DraftStore::new();
    let mut newer = Draft::new("newer typing");
    newer.content.metadata_json = Some("{\"newer\":true}".into());
    drafts.save(original.owner.clone(), newer.clone());
    let mut captures = StarterCaptures::restore(restored.starter_captures);
    let drained = captures.drain(&mut drafts);
    assert!(drained.restored.is_empty());
    assert_eq!(
        drained.conflicts,
        [(original.id.clone(), ContentConflict::Metadata)]
    );
    assert_eq!(drafts.get(&original.owner), Some(&newer));
    snapshot.starter_captures = captures.snapshot();
    snapshot.drafts = drafts
        .iter()
        .map(|(owner, draft)| (owner.clone(), draft.clone()))
        .collect();
    assert_eq!(
        round_trip(&snapshot).starter_captures.as_slice(),
        std::slice::from_ref(&original)
    );
    let id = &original.draft.content.files[1].id;
    snapshot
        .retired_blobs
        .push(*snapshot.attachments[id].blob.id());
    assert!(
        Store::capture(&snapshot).is_err(),
        "a retained failed restoration still roots its blob"
    );
    snapshot.retired_blobs.clear();
    snapshot.attachments.remove(id);
    assert!(
        Store::capture(&snapshot).is_err(),
        "a starter source cannot resolve to missing prepared bytes"
    );
    let mut wrong_parts = starter_sample();
    wrong_parts.starter_captures[0].parts.files[1].uri =
        "file:///different-target/notes.txt".into();
    assert!(
        Store::capture(&wrong_parts).is_err(),
        "prepared source and converted parts retain one target"
    );
}

#[test]
fn starter_capture_owners_and_admission_ids_share_existing_runtime_reservations() {
    let mut duplicated = starter_sample();
    duplicated
        .starter_captures
        .push(duplicated.starter_captures[0].clone());
    assert!(
        Store::capture(&duplicated).is_err(),
        "one capture per source owner"
    );
    let mut session_owner = starter_sample();
    let capture = &mut session_owner.starter_captures[0];
    capture.owner = DraftKey::new(
        capture.owner.runtime().clone(),
        capture.owner.directory(),
        DraftTarget::Session(SessionId::from("ses_wrong_owner")),
    )
    .expect("session owner");
    assert!(
        Store::capture(&session_owner).is_err(),
        "starter sources are new-session drafts"
    );
    let mut relative_owner = starter_sample();
    let capture = &mut relative_owner.starter_captures[0];
    capture.owner = DraftKey::new(
        capture.owner.runtime().clone(),
        "relative/project",
        DraftTarget::NewSession(DraftId::from("relative")),
    )
    .expect("lexical core key");
    capture.draft = Draft::default();
    capture.parts = PromptParts::default();
    assert!(
        Store::capture(&relative_owner).is_err(),
        "starter project locations must be absolute and canonical"
    );
    for use_capture_id in [false, true] {
        let mut snapshot = starter_sample();
        let runtime = snapshot.starter_captures[0].owner.runtime().clone();
        let (key, attempt) = &mut snapshot.attempts[0];
        key.runtime = runtime;
        attempt.owner = DraftKey::new(
            key.runtime.clone(),
            attempt.owner.directory(),
            DraftTarget::Session(key.session.clone()),
        )
        .expect("same-runtime session");
        let occupied = attempt.prompt.message_id.clone();
        if use_capture_id {
            snapshot.starter_captures[0].id = occupied;
        } else {
            snapshot.starter_captures[0].parts.context[0].id = occupied;
        }
        assert!(
            Store::capture(&snapshot).is_err(),
            "capture and converted context IDs are reserved runtime-wide"
        );
    }
    let mut self_reference = starter_sample();
    self_reference.starter_captures[0].parts.context[0].id =
        self_reference.starter_captures[0].id.clone();
    assert!(
        Store::capture(&self_reference).is_err(),
        "context cannot reuse the starter's instruction identity"
    );
    let mut duplicate_context = starter_sample();
    duplicate_context.starter_captures[0].parts.context[1].id =
        duplicate_context.starter_captures[0].parts.context[0]
            .id
            .clone();
    assert!(
        Store::capture(&duplicate_context).is_err(),
        "converted context identities stay unique within a capture"
    );
    let mut independent = starter_sample();
    independent.starter_captures[0].id = independent.attempts[0].1.prompt.message_id.clone();
    assert!(
        Store::capture(&independent).is_ok(),
        "distinct runtimes retain independent identity namespaces"
    );
    let mut duplicate_across_runtimes = starter_sample();
    let mut other = duplicate_across_runtimes.starter_captures[0].clone();
    other.owner = DraftKey::new(
        RuntimeId::from("another-starter-runtime"),
        "/project",
        DraftTarget::NewSession(DraftId::from("another-starter")),
    )
    .expect("other capture owner");
    other.draft = Draft::default();
    other.parts = PromptParts::default();
    duplicate_across_runtimes.starter_captures.push(other);
    assert!(
        Store::capture(&duplicate_across_runtimes).is_err(),
        "the starter owner indexes capture IDs globally and must not overwrite one on restore"
    );
}

#[test]
fn starter_capture_body_parts_and_metadata_keep_the_existing_bounds() {
    let mut preset = starter_sample();
    preset.starter_captures[0].preset_text = "x".repeat(MAX_TEXT + 1);
    assert!(Store::capture(&preset).is_err());
    let mut uri = starter_sample();
    uri.starter_captures[0].parts.files[0].uri = "x".repeat(MAX_URI + 1);
    assert!(Store::capture(&uri).is_err());
    let mut metadata = starter_sample();
    metadata.starter_captures[0].parts.metadata = Some(Metadata(serde_json::Map::from_iter([(
        "large".into(),
        serde_json::json!("x".repeat(MAX_METADATA)),
    )])));
    assert!(Store::capture(&metadata).is_err());
    let mut bad_mention = starter_sample();
    bad_mention.starter_captures[0].parts.agents[0]
        .mention
        .as_mut()
        .expect("mention")
        .text = "@same".into();
    assert!(
        Store::capture(&bad_mention).is_err(),
        "converted mentions must label original source text"
    );
    let mut aggregate = starter_sample();
    aggregate.starter_captures[0].parts.context = (0..17)
        .map(|index| SyntheticContext {
            id: MessageId::from(format!("msg_starter_large_{index}")),
            text: "x".repeat(MAX_TEXT),
            description: None,
            metadata: None,
        })
        .collect();
    assert!(
        Store::capture(&aggregate).is_err(),
        "all captured parts contribute to the 16 MiB aggregate cap"
    );
}

#[test]
fn v7_preserves_reference_payloads_context_instructions_and_exact_metadata_json() {
    let snapshot = content_sample();
    let restored = round_trip(&snapshot);
    assert_eq!(restored.drafts, snapshot.drafts);
    assert_eq!(restored.attachments, snapshot.attachments);
    assert!(restored.preparations.is_empty());
    assert!(restored.retired_blobs.is_empty());
}

#[test]
fn v7_attachment_only_draft_round_trips_without_synthetic_text() {
    let mut snapshot = content_sample();
    let draft = &mut snapshot.drafts[0].1;
    draft.text.clear();
    draft.selection = 0..0;
    draft.content.agents.clear();
    let restored = round_trip(&snapshot);
    assert!(restored.drafts[0].1.text.is_empty());
    assert!(!restored.drafts[0].1.is_empty());
    assert_eq!(restored.drafts, snapshot.drafts);
}

#[test]
fn v7_catalog_persists_digest_and_every_staging_phase_without_io() {
    for phase in [
        StagingPhase::Planned,
        StagingPhase::Writing,
        StagingPhase::Ready,
        StagingPhase::Uncertain,
    ] {
        let mut snapshot = content_sample();
        let (owner, draft) = &snapshot.drafts[0];
        let id = draft.content.files[1].id.clone();
        let attachment = snapshot
            .attachments
            .get_mut(&id)
            .expect("prepared attachment");
        attachment.staging = Some(StagingDescriptor {
            runtime: owner.runtime().clone(),
            directory: owner.directory().to_owned(),
            temporary_directory: "/tmp/opencode".into(),
            server_path: "/tmp/opencode/openchamber/0123456789abcdef/notes.txt".into(),
            uri: "file:///tmp/opencode/openchamber/0123456789abcdef/notes.txt".into(),
            filename: "notes.txt".into(),
            phase,
        });
        snapshot.preparations.push(PreparationHold {
            owner: owner.clone(),
            id: id.clone(),
            generation: 7,
            blob: *attachment.blob.id(),
        });
        let restored = round_trip(&snapshot);
        assert_eq!(restored.attachments, snapshot.attachments);
        assert_eq!(restored.preparations, snapshot.preparations);
        let original_digest = *restored.attachments[&id].blob.sha256();
        let original = value(&snapshot);
        let entry = snapshot
            .attachments
            .get_mut(&id)
            .expect("prepared attachment");
        entry.blob = BlobDescriptor::new(*entry.blob.id(), entry.blob.byte_len(), [0xa5; 32]);
        let changed = value(&snapshot);
        assert_ne!(
            original["attachments"][0]["blob"]["sha256"],
            changed["attachments"][0]["blob"]["sha256"]
        );
        let changed = round_trip(&snapshot);
        assert_ne!(changed.attachments[&id].blob.sha256(), &original_digest);
        assert_eq!(changed.attachments[&id].blob.byte_len(), 5);
    }
}

#[test]
fn pre_spool_hold_round_trips_without_fabricating_a_catalog_descriptor() {
    let (file, attachment) = prepared(7);
    let owner = sample().drafts.remove(0).0;
    let hold = PreparationHold {
        owner: owner.clone(),
        id: file.id.clone(),
        generation: 11,
        blob: *attachment.blob.id(),
    };
    let mut snapshot = JournalSnapshot {
        preparations: vec![hold.clone()],
        ..JournalSnapshot::default()
    };
    let restored = round_trip(&snapshot);
    assert_eq!(
        restored.preparations.as_slice(),
        std::slice::from_ref(&hold)
    );
    assert!(
        restored.attachments.is_empty(),
        "a proposed label is not a durable put receipt"
    );
    snapshot.retired_blobs.push(hold.blob);
    assert!(
        Store::capture(&snapshot).is_err(),
        "a pre-spool hold protects partial bytes"
    );
    snapshot.retired_blobs.clear();
    snapshot.preparations[0].id = AttachmentId::from("different-proposed-id");
    assert!(
        Store::capture(&snapshot).is_err(),
        "proposed attachment and blob labels must match"
    );
    snapshot.preparations[0] = hold.clone();
    let mut conflicting = hold.clone();
    conflicting.generation += 1;
    conflicting.owner = snapshot_owner("another-draft");
    snapshot.preparations.push(conflicting);
    assert!(
        Store::capture(&snapshot).is_err(),
        "one proposed identity cannot name conflicting holds"
    );
    snapshot.preparations.pop();
    snapshot.attachments.insert(file.id, attachment);
    let durable = round_trip(&snapshot);
    assert_eq!(durable.preparations, [hold]);
    assert_eq!(durable.attachments, snapshot.attachments);
}

fn snapshot_owner(id: &str) -> DraftKey {
    DraftKey::new(
        RuntimeId::from("other-runtime"),
        "/project",
        DraftTarget::NewSession(DraftId::from(id)),
    )
    .expect("snapshot owner")
}

#[test]
fn current_required_fields_unknown_fields_and_future_formats_fail_closed() {
    let mut snapshot = content_sample();
    let (key, creation) = crate::create_attempt::tests::sample();
    snapshot.creations.push((key, creation));
    let original = value(&snapshot);
    for (parent, field) in [
        ("", "attachments"),
        ("", "preparations"),
        ("", "retired_blobs"),
        ("", "starter_captures"),
        ("", "queue"),
        ("/drafts/0", "content"),
        ("/drafts/0/content", "owned"),
        ("/drafts/0/content/owned", "files"),
        ("/drafts/0/content/owned", "agents"),
        ("/drafts/0/content/owned", "skills"),
        ("/drafts/0/content/owned", "metadata_json"),
        ("/drafts/0/content/owned", "context"),
        ("/drafts/0/content/owned/files/0", "media_type"),
        ("/drafts/0/content/owned/files/0", "byte_len"),
        ("/drafts/0/content/owned/files/0", "source_document_id"),
        ("/drafts/0/content/owned/context/0", "instructions"),
        ("/attempts/0/draft", "content"),
        ("/attempts/0", "resources"),
        ("/attempts/0", "origin"),
        ("/attempts/0", "queue_owned"),
        ("/attempts/0", "supersedes"),
        ("/attempts/0", "reservation"),
        ("/creations/0/draft", "content"),
        ("/creations/0", "resources"),
        ("/attachments/0", "staging"),
        ("/attachments/0/blob", "sha256"),
    ] {
        let mut missing = original.clone();
        missing
            .pointer_mut(parent)
            .expect("field parent")
            .as_object_mut()
            .expect("object")
            .remove(field)
            .expect("required field exists");
        assert!(
            restore_value(&missing).is_err(),
            "reject missing {parent}/{field}"
        );
    }
    for parent in [
        "",
        "/drafts/0/content",
        "/drafts/0/content/owned",
        "/drafts/0/content/owned/files/0",
        "/attachments/0/blob",
    ] {
        let mut unknown = original.clone();
        unknown
            .pointer_mut(parent)
            .expect("parent")
            .as_object_mut()
            .expect("object")
            .insert("unknown".into(), serde_json::json!(true));
        assert!(
            restore_value(&unknown).is_err(),
            "reject unknown field at {parent}"
        );
    }
    assert!(matches!(
        decode_store(br#"{"version":9,"future":true}"#),
        Err(JournalError::UnsupportedVersion(9))
    ));
}

#[test]
fn v7_migrates_without_fabricating_queue_ownership_and_cannot_carry_v8_fields() {
    let snapshot = content_sample();
    let mut old = value(&snapshot);
    old["version"] = 7.into();
    assert!(
        restore_value(&old).is_err(),
        "a v7 envelope cannot smuggle a queue ledger"
    );
    old.as_object_mut().expect("v7 store").remove("queue");
    let restored = restore_value(&old).expect("strict v7 migration");
    assert!(restored.queue.operations().is_empty());
    assert!(restored.queue.objects().is_empty());
    assert_eq!(
        restored.drafts, snapshot.drafts,
        "full native draft content survives migration"
    );
    assert_eq!(
        restored.attachments, snapshot.attachments,
        "prepared descriptors remain unchanged"
    );
    assert_eq!(
        value(&restored)["version"],
        VERSION,
        "next save uses the current schema"
    );
}

#[test]
fn legacy_1_through_6_migrate_strictly_and_keep_frozen_prompt_identities() {
    let snapshot = multi_part();
    let current = value(&snapshot);
    for version in 1..=6 {
        let mut legacy = current.clone();
        strip_v7(&mut legacy);
        legacy["version"] = version.into();
        if version < 5 {
            for attempt in legacy["attempts"].as_array_mut().expect("attempts") {
                attempt
                    .as_object_mut()
                    .expect("attempt")
                    .remove("supersedes");
                attempt
                    .as_object_mut()
                    .expect("attempt")
                    .remove("queue_owned");
            }
        }
        if version < 4 {
            as_text_only(&mut legacy);
        }
        if version < 3 {
            legacy.as_object_mut().expect("store").remove("selections");
        }
        if version < 2 {
            legacy.as_object_mut().expect("store").remove("creations");
        }
        let first = restore_value(&legacy).expect("migrate legacy schema");
        let second = restore_value(&legacy).expect("same migration");
        assert!(
            first.starter_captures.is_empty(),
            "legacy stores contain no starter captures"
        );
        assert_eq!(
            first.drafts, snapshot.drafts,
            "unsent legacy drafts have empty content"
        );
        assert_eq!(
            first.attempts[1].1.draft, second.attempts[1].1.draft,
            "stable semantic identities"
        );
        let actual = &first.attempts[1].1;
        let expected = &snapshot.attempts[1].1;
        assert_eq!(actual.draft.text, expected.draft.text);
        assert_eq!(actual.draft.selection, expected.draft.selection);
        assert_eq!(actual.prompt.message_id, expected.prompt.message_id);
        assert_eq!(actual.prompt.model, expected.prompt.model);
        assert_eq!(actual.prompt.agent, expected.prompt.agent);
        assert_eq!(actual.prompt.delivery, expected.prompt.delivery);
        assert!(
            first.attachments.is_empty(),
            "legacy migration never fabricates blobs"
        );
        if version >= 4 {
            assert_eq!(actual.prompt.parts, expected.prompt.parts);
            assert_eq!(actual.draft.content.files.len(), 2);
            for (file, prompt_file) in actual
                .draft
                .content
                .files
                .iter()
                .zip(&expected.prompt.parts.files)
            {
                assert_eq!(
                    file.content,
                    DraftFileContent::Reference {
                        uri: prompt_file.uri.clone()
                    }
                );
                assert_eq!(file.name, prompt_file.name);
                assert!(file.media_type.is_none() && file.byte_len.is_none());
            }
            assert_eq!(
                actual.draft.content.agents,
                [AgentMention {
                    name: "plan".into(),
                    mention: None
                }]
            );
            assert_eq!(actual.draft.content.skills, expected.prompt.parts.skills);
            assert_eq!(
                actual.draft.content.context.len(),
                expected.prompt.parts.context.len()
            );
            assert!(actual.draft.content.metadata_json.is_some());
        } else {
            assert!(actual.draft.content.is_empty());
        }
        let again = round_trip(&first);
        assert_eq!(again.attempts[1].1.draft, actual.draft);
        if version < 6 {
            let mut premature = legacy.clone();
            premature["attempts"][0]["reservation"] = serde_json::Value::Null;
            assert!(
                restore_value(&premature).is_err(),
                "a null future field is still a schema violation"
            );
        }
        let mut unknown = legacy;
        unknown["attempts"][0]["draft"]["content"] = serde_json::json!({});
        assert!(
            restore_value(&unknown).is_err(),
            "legacy schema rejects v7 content"
        );
    }
}

#[test]
fn legacy_creations_recover_parts_without_replacing_the_source_text_or_caret() {
    for version in [4, 5, 6] {
        let (key, mut creation) = crate::create_attempt::tests::sample();
        creation.draft = Draft::new("x");
        creation.prompt.text = "@plan rendered starter".into();
        creation.prompt.parts = multi_part().attempts.remove(1).1.prompt.parts;
        let expected = creation.clone();
        let snapshot = JournalSnapshot {
            creations: vec![(key, creation)],
            ..JournalSnapshot::default()
        };
        let mut legacy = value(&snapshot);
        strip_v7(&mut legacy);
        legacy["version"] = version.into();
        let restored = restore_value(&legacy).expect("legacy creation with rendered text");
        let actual = &restored.creations[0].1;
        assert_eq!(actual.draft.text, "x");
        assert_eq!(actual.draft.selection, 1..1);
        assert_eq!(actual.prompt.parts, expected.prompt.parts);
        assert_eq!(actual.prompt.message_id, expected.prompt.message_id);
        assert_eq!(actual.request, expected.request);
        assert_eq!(actual.draft.content.files.len(), 2);
        assert_eq!(actual.draft.content.context.len(), 2);
        assert_eq!(
            actual.draft.content.agents,
            [AgentMention {
                name: "plan".into(),
                mention: None
            }]
        );
        assert_eq!(round_trip(&restored).creations[0].1.draft, actual.draft);
        let recovered = JournalSnapshot {
            drafts: vec![(actual.source.clone(), actual.draft.clone())],
            ..JournalSnapshot::default()
        };
        assert_eq!(
            round_trip(&recovered).drafts,
            recovered.drafts,
            "recovered capture is a valid independent composer draft"
        );
    }
}

#[test]
fn legacy_mentions_keep_only_exact_ordered_utf16_anchors_in_the_source_draft() {
    for (source, start, end, anchored) in [
        ("@plan", 0, 5, true),
        ("@same", 0, 5, false),
        ("x", 0, 5, false),
        ("👩@plan", 1, 6, false),
        ("👩 @plan", 3, 8, true),
    ] {
        let mut snapshot = sample();
        let attempt = &mut snapshot.attempts[0].1;
        attempt.draft = Draft::new(source);
        attempt.prompt.text = format!("{}@plan rendered", " ".repeat(start as usize));
        let original = AgentMention {
            name: "plan".into(),
            mention: Some(Mention {
                start,
                end,
                text: "@plan".into(),
            }),
        };
        attempt.prompt.parts.agents = vec![original.clone()];
        attempt.optimistic.parts[0] = Part::text(
            attempt.optimistic.parts[0].id.clone(),
            attempt.prompt.text.clone(),
        );
        let mut legacy = value(&snapshot);
        strip_v7(&mut legacy);
        legacy["version"] = 6.into();
        let restored = restore_value(&legacy).expect("legacy source and rendered prompt");
        let actual = &restored.attempts[0].1;
        assert_eq!(
            actual.prompt.parts.agents.as_slice(),
            std::slice::from_ref(&original),
            "frozen mentions stay exact"
        );
        assert_eq!(
            actual.draft.content.agents[0].mention,
            anchored.then_some(original.mention).flatten()
        );
        let returned = JournalSnapshot {
            drafts: vec![(actual.owner.clone(), actual.draft.clone())],
            ..JournalSnapshot::default()
        };
        assert_eq!(round_trip(&returned).drafts, returned.drafts);
    }
}

#[test]
fn v7_owned_mentions_cannot_validate_against_different_frozen_text() {
    let mut snapshot = sample();
    let attempt = &mut snapshot.attempts[0].1;
    attempt.draft = Draft::new("x");
    attempt.prompt.text = "@plan".into();
    let agent = AgentMention {
        name: "plan".into(),
        mention: Some(Mention {
            start: 0,
            end: 5,
            text: "@plan".into(),
        }),
    };
    attempt.prompt.parts.agents = vec![agent.clone()];
    attempt.optimistic.parts[0] = Part::text(attempt.optimistic.parts[0].id.clone(), "@plan");
    let owner = attempt.owner.clone();
    let mut stored = value(&snapshot);
    let mut invalid = Draft::new("@plan");
    invalid.content.agents.push(agent);
    stored["attempts"][0]["draft"]["content"] = serde_json::to_value(StoredDraft::capture(
        &owner, &invalid,
    ))
    .expect("owned draft")["content"]
        .clone();
    assert!(
        restore_value(&stored).is_err(),
        "owned draft anchors must fit its own source text"
    );
}

#[test]
fn repaired_legacy_mentions_coalesce_equal_unanchored_references_and_keep_frozen_tokens() {
    let mut snapshot = sample();
    let attempt = &mut snapshot.attempts[0].1;
    attempt.draft = Draft::new("x");
    attempt.prompt.text = "@plan @plan".into();
    attempt.prompt.parts.agents = [0, 6]
        .into_iter()
        .map(|start| AgentMention {
            name: "plan".into(),
            mention: Some(Mention {
                start,
                end: start + 5,
                text: "@plan".into(),
            }),
        })
        .collect();
    attempt.optimistic.parts[0] = Part::text(attempt.optimistic.parts[0].id.clone(), "@plan @plan");
    let mut legacy = value(&snapshot);
    strip_v7(&mut legacy);
    legacy["version"] = 6.into();
    let restored = restore_value(&legacy).expect("repair repeated rendered mentions");
    assert_eq!(restored.attempts[0].1.prompt.parts.agents.len(), 2);
    assert_eq!(
        restored.attempts[0].1.draft.content.agents,
        [AgentMention {
            name: "plan".into(),
            mention: None
        }]
    );
}

#[test]
fn legacy_large_context_migrates_saves_and_reopens_under_the_unchanged_file_cap() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut snapshot = sample();
    snapshot.attempts[0].1.prompt.parts.context = (0..9)
        .map(|index| SyntheticContext {
            id: MessageId::from(format!("msg_large_context_{index}")),
            text: "x".repeat(MAX_TEXT),
            description: None,
            metadata: None,
        })
        .collect();
    let current = value(&snapshot);
    for version in [4, 5, 6] {
        let mut legacy = current.clone();
        strip_v7(&mut legacy);
        legacy["version"] = version.into();
        if version < 5 {
            legacy["attempts"][0]
                .as_object_mut()
                .expect("attempt")
                .remove("supersedes");
            legacy["attempts"][0]
                .as_object_mut()
                .expect("attempt")
                .remove("queue_owned");
        }
        let legacy_bytes = serde_json::to_vec(&legacy).expect("legacy JSON");
        assert!((legacy_bytes.len() as u64) < MAX_FILE);
        write_raw(&path, &legacy_bytes);
        let (mut journal, loaded_value) =
            MutationJournal::open(&path).expect("valid large legacy store");
        let migrated = loaded(loaded_value);
        let capture = &migrated.attempts[0].1;
        assert_eq!(capture.draft.content.context.len(), 9);
        for (context, frozen) in capture
            .draft
            .content
            .context
            .iter()
            .zip(&capture.prompt.parts.context)
        {
            assert_eq!(context.text, frozen.text);
        }
        assert_eq!(capture.draft.text, snapshot.attempts[0].1.draft.text);
        assert_eq!(
            capture.draft.selection,
            snapshot.attempts[0].1.draft.selection
        );
        assert!(matches!(
            journal.save(&migrated),
            Ok(JournalCommit::Durable)
        ));
        drop(journal);
        let bytes = fs::read(&path).expect("compact v7 store");
        assert!(
            (bytes.len() as u64) < MAX_FILE,
            "recovered context is stored only in the enclosing prompt"
        );
        let stored: serde_json::Value = serde_json::from_slice(&bytes).expect("v7 JSON");
        assert_eq!(stored["attempts"][0]["draft"]["content"], "captured_prompt");
        let (_journal, loaded_value) = MutationJournal::open(&path).expect("reopen compact v7");
        let reopened = loaded(loaded_value);
        assert_eq!(reopened.attempts[0].1.draft, capture.draft);
        assert_eq!(reopened.attempts[0].1.prompt.parts, capture.prompt.parts);
        assert_eq!(
            reopened.attempts[0].1.prompt.message_id,
            capture.prompt.message_id
        );
        assert_eq!(reopened.attempts[0].1.prompt.model, capture.prompt.model);
        assert_eq!(reopened.attempts[0].1.prompt.agent, capture.prompt.agent);
        assert_eq!(
            reopened.attempts[0].1.prompt.delivery,
            capture.prompt.delivery
        );
        let mut nonidentical = reopened;
        nonidentical.attempts[0].1.draft.content.context[0].instructions =
            Some("source-only instructions".into());
        assert!(
            Store::capture(&nonidentical).is_err(),
            "nonidentical content must own and charge its full physical copy"
        );
        let standalone = JournalSnapshot {
            drafts: vec![(capture.owner.clone(), capture.draft.clone())],
            ..JournalSnapshot::default()
        };
        let mut duplicated = stored;
        duplicated["attempts"][0]["draft"]["content"] =
            value(&standalone)["drafts"][0]["content"].clone();
        assert!(
            serde_json::from_value::<Store>(duplicated)
                .expect("explicit owned schema")
                .into_snapshot()
                .is_err(),
            "loading an explicit second physical copy cannot borrow backref accounting"
        );
    }
}

#[test]
fn captured_prompt_content_cannot_escape_its_enclosing_attempt_or_creation() {
    let mut snapshot = value(&sample());
    snapshot["drafts"][0]["content"] = "captured_prompt".into();
    assert!(
        restore_value(&snapshot).is_err(),
        "ordinary drafts have no enclosing frozen prompt"
    );
    let mut snapshot = value(&sample());
    snapshot["attempts"][0]["draft"]["content"] =
        serde_json::json!({"captured_prompt":"other-owner"});
    assert!(
        restore_value(&snapshot).is_err(),
        "backrefs carry no arbitrary owner or message identity"
    );
    let mut snapshot = value(&sample());
    snapshot["attempts"][0]["draft"]
        .as_object_mut()
        .expect("capture")
        .remove("content");
    assert!(
        restore_value(&snapshot).is_err(),
        "v7 content remains required"
    );
}

#[test]
fn refused_send_with_a_conflicting_draft_remains_owned_and_durable_during_abort() {
    let mut snapshot = sample();
    let (key, mut captured) = snapshot.attempts.pop().expect("captured send");
    let (file, attachment) = prepared(1);
    captured.resources = vec![file.id.clone()];
    captured.draft.content.metadata_json = Some("{\"capture\":true}".into());
    let original = captured.clone();
    let mut newer = Draft::new("newer typing");
    newer.content.metadata_json = Some("{\"newer\":true}".into());
    let mut attempts = SendAttempts::default();
    assert!(attempts.begin(key.clone(), captured));
    let removed = attempts
        .finish(
            &key,
            &original.prompt.message_id,
            Err(openchamber_native_transport::prompt::PromptAdmission::Rejected),
        )
        .expect("definite refusal returns the capture");
    assert!(crate::send_attempt::restore_failed(Some(&newer), &removed.draft).is_err());
    assert!(
        attempts.begin_abort(key.clone()),
        "abort may independently own this key"
    );
    assert!(attempts.retain_failed(key.clone(), removed).is_ok());
    assert!(
        !attempts.is_sending(&key, &original.prompt.message_id),
        "retention starts no replay task"
    );
    snapshot.drafts.push((original.owner.clone(), newer));
    snapshot.attachments.insert(file.id, attachment);
    snapshot.attempts = attempts.snapshot();
    let restored = round_trip(&snapshot);
    let held = &restored.attempts[0].1;
    assert_eq!(restored.attempts[0].0, key);
    assert_eq!(held.admission, Admission::Uncertain);
    assert_eq!(held.draft, original.draft);
    assert_eq!(held.prompt.message_id, original.prompt.message_id);
    assert_eq!(held.prompt.text, original.prompt.text);
    assert_eq!(held.prompt.parts, original.prompt.parts);
    assert_eq!(held.prompt.model, original.prompt.model);
    assert_eq!(held.prompt.agent, original.prompt.agent);
    assert_eq!(held.prompt.delivery, original.prompt.delivery);
    assert_eq!(held.resources, original.resources);
    assert_eq!(held.optimistic, original.optimistic);
    let attempts =
        SendAttempts::from_snapshot(restored.attempts).expect("restore retained refusal");
    assert!(!attempts.is_active(&key));
    assert!(
        !attempts.can_send(&key),
        "an explicit recovery action must resolve the conflict first"
    );
}

#[test]
fn legacy_recovery_ids_keep_reused_message_ids_in_distinct_runtime_namespaces() {
    let mut snapshot = multi_part();
    let mut other = snapshot.attempts.clone();
    for (key, attempt) in &mut other {
        key.runtime = RuntimeId::from("other-runtime");
        attempt.owner = DraftKey::new(
            key.runtime.clone(),
            attempt.owner.directory(),
            DraftTarget::Session(key.session.clone()),
        )
        .expect("other runtime owner");
        for file in &mut attempt.prompt.parts.files {
            file.uri = format!("https://example.invalid/other/{}", file.uri.len());
        }
    }
    snapshot.attempts.extend(other);
    let mut legacy = value(&snapshot);
    strip_v7(&mut legacy);
    legacy["version"] = 6.into();
    let restored = restore_value(&legacy).expect("independent runtime namespaces");
    assert_ne!(
        restored.attempts[1].1.draft.content.files[0].id,
        restored.attempts[3].1.draft.content.files[0].id
    );
}

#[test]
fn v6_reservation_and_v5_queue_ownership_survive_migration() {
    for version in [5, 6] {
        let mut snapshot = sample();
        let attempt = &mut snapshot.attempts[0].1;
        attempt.supersedes = false;
        attempt.queue_owned = true;
        if version == 6 {
            attempt.reservation = Some(crate::send_attempt::QueueReservation {
                items: vec!["queued-item".into()],
                composer: "original composer".into(),
            });
        }
        let mut legacy = value(&snapshot);
        strip_v7(&mut legacy);
        legacy["version"] = version.into();
        let restored = restore_value(&legacy).expect("migrate queue send");
        let attempt = &restored.attempts[0].1;
        assert!(!attempt.supersedes && attempt.queue_owned);
        assert_eq!(attempt.reservation.is_some(), version == 6);
        if let Some(intent) = &attempt.reservation {
            assert_eq!(intent.items, ["queued-item"]);
            assert_eq!(intent.composer, "original composer");
        }
        legacy["attempts"][0]
            .as_object_mut()
            .expect("attempt")
            .remove("queue_owned");
        assert!(
            restore_value(&legacy).is_err(),
            "queue ownership must be stated explicitly"
        );
    }
}

#[test]
fn v7_rejects_unresolved_roots_conflicting_file_identities_and_live_retirement() {
    let mut missing = content_sample();
    missing.attachments.clear();
    assert!(
        Store::capture(&missing).is_err(),
        "prepared draft root resolves"
    );
    let mut send_root = sample();
    send_root.attempts[0]
        .1
        .resources
        .push(AttachmentId::from("missing"));
    assert!(
        Store::capture(&send_root).is_err(),
        "send resource resolves"
    );
    let (key, mut creation) = crate::create_attempt::tests::sample();
    creation.resources.push(AttachmentId::from("missing"));
    let creation_root = JournalSnapshot {
        creations: vec![(key, creation)],
        ..JournalSnapshot::default()
    };
    assert!(
        Store::capture(&creation_root).is_err(),
        "creation resource resolves"
    );
    let mut snapshot = content_sample();
    let file = snapshot.drafts[0].1.content.files[1].clone();
    let attachment = snapshot.attachments[&file.id].clone();
    snapshot.retired_blobs.push(*attachment.blob.id());
    assert!(
        Store::capture(&snapshot).is_err(),
        "live catalog cannot be retired"
    );
    snapshot.retired_blobs.clear();
    snapshot.preparations.push(PreparationHold {
        owner: snapshot.drafts[0].0.clone(),
        id: file.id.clone(),
        generation: 0,
        blob: attachment_catalog::blob_id("00000000000000000000000000000002").expect("other blob"),
    });
    assert!(
        Store::capture(&snapshot).is_err(),
        "hold must resolve the exact blob"
    );
    snapshot.preparations.clear();
    let mut conflict = snapshot.drafts[0].1.clone();
    conflict.content.files[1].name = Some("different.txt".into());
    let owner = snapshot.attempts[0].1.owner.clone();
    snapshot.drafts.push((owner, conflict));
    assert!(
        Store::capture(&snapshot).is_err(),
        "one identity has immutable facts across owners"
    );
}

#[test]
fn v7_retired_intents_round_trip_and_reject_duplicate_ids() {
    let mut snapshot = sample();
    snapshot
        .retired_blobs
        .push(attachment_catalog::blob_id("00000000000000000000000000000002").expect("blob"));
    assert_eq!(round_trip(&snapshot).retired_blobs, snapshot.retired_blobs);
    snapshot.retired_blobs.push(snapshot.retired_blobs[0]);
    assert!(Store::capture(&snapshot).is_err());
    let mut duplicated = value(&content_sample());
    let entry = duplicated["attachments"][0].clone();
    duplicated["attachments"]
        .as_array_mut()
        .expect("catalog")
        .push(entry);
    assert!(
        restore_value(&duplicated).is_err(),
        "duplicate catalog IDs are rejected before map insertion can hide them"
    );
}

#[test]
fn v7_rejects_mismatched_blob_identity_length_staging_targets_and_resource_duplicates() {
    let mut snapshot = content_sample();
    let (owner, draft) = &snapshot.drafts[0];
    let id = draft.content.files[1].id.clone();
    snapshot
        .attachments
        .get_mut(&id)
        .expect("attachment")
        .staging = Some(StagingDescriptor {
        runtime: owner.runtime().clone(),
        directory: owner.directory().to_owned(),
        temporary_directory: "/tmp/opencode".into(),
        server_path: "/tmp/opencode/openchamber/0123456789abcdef/notes.txt".into(),
        uri: "file:///tmp/opencode/openchamber/0123456789abcdef/notes.txt".into(),
        filename: "notes.txt".into(),
        phase: StagingPhase::Uncertain,
    });
    let original = value(&snapshot);
    for (path, replacement) in [
        (
            "/attachments/0/blob/id",
            serde_json::json!("00000000000000000000000000000002"),
        ),
        ("/attachments/0/blob/byte_len", serde_json::json!(6)),
        ("/attachments/0/blob/sha256", serde_json::json!([1, 2])),
        (
            "/attachments/0/staging/runtime",
            serde_json::json!("another-runtime"),
        ),
        (
            "/attachments/0/staging/directory",
            serde_json::json!("/another-project"),
        ),
        (
            "/attachments/0/staging/temporary_directory",
            serde_json::json!("/workspace"),
        ),
        (
            "/attachments/0/staging/server_path",
            serde_json::json!("/tmp/opencode/openchamber/short/notes.txt"),
        ),
        (
            "/attachments/0/staging/server_path",
            serde_json::json!("/tmp/opencode/openchamber/0123456789abcdef/../notes.txt"),
        ),
        (
            "/attachments/0/staging/uri",
            serde_json::json!("file:///workspace/notes.txt"),
        ),
        (
            "/attachments/0/staging/filename",
            serde_json::json!("NUL.txt"),
        ),
        ("/attachments/0/staging/phase", serde_json::json!("unknown")),
    ] {
        let mut malformed = original.clone();
        *malformed.pointer_mut(path).expect("field") = replacement;
        assert!(restore_value(&malformed).is_err(), "reject {path}");
    }
    let (file, attachment) = prepared(2);
    snapshot.attachments.insert(file.id.clone(), attachment);
    snapshot.attempts[0].1.resources = vec![file.id.clone(), file.id];
    assert!(
        Store::capture(&snapshot).is_err(),
        "duplicate resource roots are malformed"
    );
}

#[test]
fn v7_creation_to_send_handoff_preserves_resources_in_one_valid_snapshot() {
    let (key, mut creation) = crate::create_attempt::tests::sample();
    let (file, attachment) = prepared(1);
    creation.resources.push(file.id.clone());
    let captured = creation.draft.clone();
    let mut snapshot = JournalSnapshot {
        creations: vec![(key.clone(), creation)],
        ..JournalSnapshot::default()
    };
    snapshot.attachments.insert(file.id.clone(), attachment);
    let restored = round_trip(&snapshot);
    assert_eq!(
        restored.creations[0].1.resources.as_slice(),
        std::slice::from_ref(&file.id)
    );
    assert_eq!(
        restored.creations[0].1.draft, captured,
        "starter resources need not be in the original capture"
    );
    let (_, creation) = snapshot.creations.pop().expect("handoff source");
    let mut send = attempt_tests::attempt(&key);
    send.origin = SendOrigin::Created;
    send.owner = DraftKey::new(
        key.runtime.clone(),
        creation.source.directory(),
        DraftTarget::Session(key.session.clone()),
    )
    .expect("session owner");
    send.draft = creation.draft;
    send.resources = creation.resources;
    send.prompt = creation.prompt;
    send.optimistic.info.id = send.prompt.message_id.clone();
    send.optimistic.parts[0] = Part::text(
        send.optimistic.parts[0].id.clone(),
        send.prompt.text.clone(),
    );
    snapshot.attempts.push((key, send));
    let restored = round_trip(&snapshot);
    assert!(restored.creations.is_empty());
    assert_eq!(restored.attempts[0].1.resources, [file.id]);
    assert_eq!(restored.attempts[0].1.draft, captured);
    assert_eq!(restored.attachments, snapshot.attachments);
}

#[test]
fn v7_rejects_metadata_nonobjects_duplicate_content_ids_and_invalid_offsets() {
    for path in [
        "/drafts/0/content/owned/metadata_json",
        "/drafts/0/content/owned/context/0/metadata_json",
    ] {
        for invalid in ["[]", "null", "42", "true", "{bad"] {
            let mut malformed = value(&content_sample());
            *malformed.pointer_mut(path).expect("metadata field") = invalid.into();
            assert!(
                restore_value(&malformed).is_err(),
                "metadata must be an object"
            );
        }
    }
    for list in ["files", "context", "skills", "agents"] {
        let mut malformed = value(&content_sample());
        let entries = malformed["drafts"][0]["content"]["owned"][list]
            .as_array_mut()
            .expect("content list");
        entries.push(entries[0].clone());
        assert!(
            restore_value(&malformed).is_err(),
            "reject duplicate {list}"
        );
    }
    for (start, end) in [(6, 5), (0, 99), (8, 9)] {
        let mut malformed = value(&content_sample());
        malformed["drafts"][0]["content"]["owned"]["agents"][0]["mention"]["start"] = start.into();
        malformed["drafts"][0]["content"]["owned"]["agents"][0]["mention"]["end"] = end.into();
        assert!(
            restore_value(&malformed).is_err(),
            "reject out-of-range or split UTF-16 offsets"
        );
    }
    let mut oversized = content_sample();
    oversized.drafts[0].1.content.context[0].instructions = Some("a".repeat(MAX_TEXT + 1));
    assert!(Store::capture(&oversized).is_err());
    let mut aggregate = JournalSnapshot::default();
    for index in 0..17 {
        let owner = DraftKey::new(
            RuntimeId::from("runtime"),
            "/project",
            DraftTarget::NewSession(DraftId::from(format!("draft-{index}"))),
        )
        .expect("owner");
        let mut draft = Draft::default();
        draft.content.context.push(DraftContext {
            id: ContextItemId::from(format!("context-{index}")),
            text: "x".repeat(MAX_TEXT),
            description: None,
            metadata_json: None,
            instructions: None,
        });
        aggregate.drafts.push((owner, draft));
    }
    assert!(
        Store::capture(&aggregate).is_err(),
        "full semantic content contributes to the aggregate bound"
    );
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!(
            "/tmp/opencode/mutation-journal-{}-{id}",
            std::process::id()
        ));
        private_directory(&path).expect("create private test directory");
        Self(path)
    }

    fn path(&self) -> PathBuf {
        self.0.join("mutations.json")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sample() -> JournalSnapshot {
    let key = attempt_tests::key("runtime", "session");
    let mut attempt = attempt_tests::attempt(&key);
    attempt.draft = Draft {
        text: "é👩🏽‍💻\n漢字\0 exact \r\n".to_owned(),
        selection: 2..17,
        ..Draft::default()
    };
    attempt.prompt.text = attempt.draft.text.clone();
    attempt.prompt.delivery = Some(Delivery::Queue);
    attempt.optimistic.parts[0] = Part::text(
        attempt.optimistic.parts[0].id.clone(),
        attempt.draft.text.clone(),
    );
    let owner = DraftKey::new(
        RuntimeId::from("other-runtime"),
        "/project",
        DraftTarget::NewSession(DraftId::from("draft")),
    )
    .expect("absolute directory");
    JournalSnapshot {
        drafts: vec![(owner, Draft::new("unsent λ"))],
        attempts: vec![(key, attempt)],
        creations: Vec::new(),
        selections: Vec::new(),
        ..JournalSnapshot::default()
    }
}

#[test]
fn staged_selection_opened_before_handoff_round_trips_without_duplicate_owners() {
    use openchamber_native_core::composer_selection::{
        ComposerSelections, CreationSource, Effort, SelectionContext, SelectionOwner,
    };
    use openchamber_native_core::ids::ProjectId;
    use openchamber_native_core::send_config::{ModelChoice, Selection, SelectionDefaults};
    let (key, attempt) = crate::create_attempt::tests::sample();
    let source = SelectionOwner {
        draft: attempt.source.clone(),
        project: Some(ProjectId::from(attempt.project.clone())),
    };
    let target = SelectionOwner {
        draft: DraftKey::new(
            key.runtime.clone(),
            source.draft.directory(),
            DraftTarget::Session(key.session.clone()),
        )
        .expect("target"),
        project: source.project.clone(),
    };
    let defaults = SelectionDefaults::default();
    let context = SelectionContext {
        agents: &[],
        providers: &[],
        defaults: &defaults,
        catalog_authoritative: false,
    };
    let mut choices = ComposerSelections::default();
    choices.open(source.clone(), &context);
    let captured = ModelChoice {
        provider_id: "p".into(),
        model_id: "captured".into(),
        variant: None,
    };
    choices.pick_model(&source, captured.clone(), Some(Effort::Default), &context);
    assert!(choices.stage_creation(&source, target.clone()));
    choices.open(target.clone(), &context);
    assert_eq!(
        choices
            .get(&target)
            .expect("opened destination")
            .selection()
            .model,
        Some(captured),
        "opening a pending creation starts with its captured choices"
    );
    let newer = ModelChoice {
        provider_id: "p".into(),
        model_id: "newer".into(),
        variant: None,
    };
    choices.pick_model(&target, newer.clone(), Some(Effort::Default), &context);
    let mut snapshot = JournalSnapshot {
        drafts: vec![],
        attempts: vec![],
        creations: vec![(key, attempt)],
        selections: choices.snapshot(),
        ..JournalSnapshot::default()
    };
    let round_trip = |snapshot: &JournalSnapshot| {
        let bytes =
            serde_json::to_vec(&Store::capture(snapshot).expect("valid journal during handoff"))
                .expect("encode");
        serde_json::from_slice::<Store>(&bytes)
            .expect("decode")
            .into_snapshot()
            .expect("valid persisted journal")
    };
    snapshot = round_trip(&snapshot);
    choices = ComposerSelections::restore(snapshot.selections);
    choices.open(target.clone(), &context);
    choices.commit_creation(
        target.clone(),
        Selection::default(),
        CreationSource::Continued,
    );
    snapshot.creations.clear();
    snapshot.selections = choices.snapshot();
    let saved = round_trip(&snapshot);
    assert!(
        saved
            .selections
            .iter()
            .all(|(_, state)| state.staged_from.is_none())
    );
    assert_eq!(
        choices
            .get(&target)
            .expect("session choice")
            .selection()
            .model,
        Some(newer)
    );
}

#[test]
fn queue_row_supersession_survives_storage_and_legacy_defaults_to_true() {
    let fixture = Scratch::new();
    let path = fixture.path();
    let (mut journal, _) = MutationJournal::open(&path).expect("open");
    let mut snapshot = sample();
    snapshot.attempts[0].1.supersedes = false;
    snapshot.attempts[0].1.queue_owned = true;
    journal.save(&snapshot).expect("save row send");
    drop(journal);
    let (journal, value) = MutationJournal::open(&path).expect("reload");
    let restored = loaded(value);
    assert!(!restored.attempts[0].1.supersedes);
    assert!(restored.attempts[0].1.queue_owned);
    drop(journal);
    let current: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
    for version in 1..=4 {
        let mut old = current.clone();
        strip_v7(&mut old);
        old["version"] = version.into();
        for entry in old["attempts"].as_array_mut().expect("attempts") {
            entry.as_object_mut().expect("attempt").remove("supersedes");
            entry
                .as_object_mut()
                .expect("attempt")
                .remove("queue_owned");
        }
        if version < 4 {
            as_text_only(&mut old);
        }
        if version < 3 {
            old.as_object_mut().expect("store").remove("selections");
        }
        if version < 2 {
            old.as_object_mut().expect("store").remove("creations");
        }
        fs::write(&path, serde_json::to_vec(&old).expect("JSON")).expect("write legacy");
        let (journal, value) = MutationJournal::open(&path).expect("migrate legacy");
        let restored = loaded(value);
        assert!(
            restored.attempts[0].1.supersedes,
            "version {version} defaults true"
        );
        assert!(
            !restored.attempts[0].1.queue_owned,
            "legacy sends have no reservation"
        );
        drop(journal);
    }
    let mut missing = current;
    missing["attempts"][0]
        .as_object_mut()
        .expect("attempt")
        .remove("supersedes");
    fs::write(&path, serde_json::to_vec(&missing).expect("JSON")).expect("write malformed");
    assert!(
        MutationJournal::open(&path).is_err(),
        "v5 must state its policy"
    );
}

#[test]
fn journal_v1_migrates_without_losing_existing_sends() {
    let fixture = Scratch::new();
    let path = fixture.path();
    let (mut journal, _) = MutationJournal::open(&path).expect("open");
    journal.save(&sample()).expect("v2 save");
    drop(journal);
    let mut stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
    as_text_only(&mut stored);
    stored["version"] = 1.into();
    stored.as_object_mut().expect("object").remove("creations");
    stored.as_object_mut().expect("object").remove("selections");
    for attempt in stored["attempts"].as_array_mut().expect("attempts") {
        attempt.as_object_mut().expect("attempt").remove("origin");
    }
    fs::write(&path, serde_json::to_vec(&stored).expect("JSON")).expect("legacy write");
    let (mut journal, value) = MutationJournal::open(&path).expect("legacy loads");
    let snapshot = loaded(value);
    assert!(snapshot.creations.is_empty());
    assert_eq!(snapshot.attempts[0].1.origin, SendOrigin::Existing);
    assert_eq!(
        snapshot.attempts[0].1.prompt.text,
        sample().attempts[0].1.prompt.text
    );
    journal.save(&snapshot).expect("migrated save");
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
    assert_eq!(stored["version"], VERSION);
}

#[test]
fn creation_journal_round_trips_all_phases_and_newer_identical_typing() {
    for phase in [
        CreationPhase::Preparing,
        CreationPhase::Creating,
        CreationPhase::Uncertain,
        CreationPhase::Confirming,
        CreationPhase::Confirmed,
        CreationPhase::Mismatch,
        CreationPhase::Deleted,
    ] {
        let (key, mut attempt) = crate::create_attempt::tests::sample();
        attempt.phase = phase;
        let request = attempt.request.clone();
        let snapshot = JournalSnapshot {
            drafts: vec![(attempt.source.clone(), attempt.draft.clone())],
            attempts: vec![],
            creations: vec![(key, attempt)],
            selections: Vec::new(),
            ..JournalSnapshot::default()
        };
        let restored = round_trip(&snapshot);
        let held = &restored.creations[0].1;
        assert_eq!(held.request, request);
        assert_eq!(
            held.draft, restored.drafts[0].1,
            "identical newer typing remains distinct"
        );
        assert_eq!(
            held.phase,
            if matches!(phase, CreationPhase::Preparing | CreationPhase::Creating) {
                CreationPhase::Uncertain
            } else if phase == CreationPhase::Confirming {
                CreationPhase::Confirmed
            } else {
                phase
            }
        );
        assert!(held.navigation.is_none());
        assert_eq!(held.prompt.model.as_ref().expect("Auto").model_id, "auto");
    }
}

#[test]
fn creation_journal_rejects_conflicting_owners_configuration_and_identities() {
    let (key, attempt) = crate::create_attempt::tests::sample();
    let mut snapshot = JournalSnapshot {
        drafts: vec![],
        attempts: vec![],
        creations: vec![(key.clone(), attempt.clone())],
        selections: Vec::new(),
        ..JournalSnapshot::default()
    };
    snapshot.creations.push((key.clone(), attempt));
    assert!(Store::capture(&snapshot).is_err());
    snapshot.creations.pop();
    snapshot.creations[0].1.prompt.agent = Some("different".into());
    assert!(Store::capture(&snapshot).is_err());
    snapshot.creations[0].1.prompt.agent = Some("build".into());
    snapshot
        .attempts
        .push((key.clone(), attempt_tests::attempt(&key)));
    assert!(Store::capture(&snapshot).is_err());
}

fn loaded(value: LoadedJournal) -> JournalSnapshot {
    match value {
        LoadedJournal::Present(snapshot) => *snapshot,
        LoadedJournal::Missing => panic!("expected persisted snapshot"),
    }
}

#[test]
fn journal_selection_default_and_memory_round_trip_and_malformed_rejection() {
    use openchamber_native_core::composer_selection::{
        Effort, ManualSelection, SelectionOwner, SelectionSnapshot,
    };
    use openchamber_native_core::send_config::{ModelChoice, Selection};
    let mut snapshot = sample();
    let owner = SelectionOwner {
        draft: snapshot.drafts[0].0.clone(),
        project: None,
    };
    let model = ModelChoice {
        provider_id: "p".into(),
        model_id: "m".into(),
        variant: Some("high".into()),
    };
    let state = SelectionSnapshot {
        inherited: Selection {
            agent_name: Some("build".into()),
            model: Some(model.clone()),
        },
        effort: Effort::Default,
        manual: ManualSelection {
            agent: false,
            model: true,
        },
        agent_models: vec![("build".into(), model)],
        efforts: vec![(
            Some("build".into()),
            "p".into(),
            "m".into(),
            Effort::Default,
        )],
        ..SelectionSnapshot::default()
    };
    snapshot.selections.push((owner, state));
    let bytes =
        serde_json::to_vec(&Store::capture(&snapshot).expect("valid capture")).expect("serialize");
    let stored: Store = serde_json::from_slice(&bytes).expect("decode");
    let restored = stored.into_snapshot().expect("restore");
    assert_eq!(
        snapshot.selections, restored.selections,
        "Default remains separate from Inherit"
    );
    let mut invalid = restored;
    invalid.selections.push(invalid.selections[0].clone());
    assert!(
        Store::capture(&invalid).is_err(),
        "duplicate owners never get a writer"
    );
    invalid.selections.pop();
    let duplicate = invalid.selections[0].1.efforts[0].clone();
    invalid.selections[0].1.efforts.push(duplicate);
    assert!(
        Store::capture(&invalid).is_err(),
        "duplicate memory keys are rejected"
    );
    let mut malformed: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
    malformed["selections"][0]["effort"] = serde_json::json!({"named": ""});
    assert!(
        serde_json::from_value::<Store>(malformed)
            .expect("shape")
            .into_snapshot()
            .is_err()
    );
}

#[test]
fn journal_v2_loads_and_v3_requires_selection_field() {
    let fixture = Scratch::new();
    let path = fixture.path();
    let (mut journal, _) = MutationJournal::open(&path).expect("open");
    journal.save(&sample()).expect("save");
    drop(journal);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
    as_text_only(&mut value);
    value["version"] = 3.into();
    value.as_object_mut().expect("object").remove("selections");
    fs::write(&path, serde_json::to_vec(&value).expect("JSON")).expect("write");
    assert!(
        MutationJournal::open(&path).is_err(),
        "missing v3 selection field is malformed"
    );
    value["version"] = 2.into();
    fs::write(&path, serde_json::to_vec(&value).expect("JSON")).expect("write");
    let (mut journal, restored) = MutationJournal::open(&path).expect("v2 loads");
    let restored = loaded(restored);
    assert!(restored.selections.is_empty());
    assert_eq!(restored.attempts.len(), 1);
    journal.save(&restored).expect("resave");
}

/// Rewrites a version 4 store's prompts in the text-only layout versions 1 to
/// 3 wrote, so legacy files come from the same helpers as current ones.
fn as_text_only(stored: &mut serde_json::Value) {
    strip_v7(stored);
    for list in ["attempts", "creations"] {
        let Some(entries) = stored
            .get_mut(list)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for entry in entries {
            entry.as_object_mut().expect("entry").remove("supersedes");
            entry.as_object_mut().expect("entry").remove("queue_owned");
            entry.as_object_mut().expect("entry").remove("reservation");
            let prompt = entry["prompt"].as_object_mut().expect("prompt object");
            for field in ["files", "agents", "skills", "metadata", "context"] {
                prompt.remove(field);
            }
        }
    }
}

fn strip_v7(stored: &mut serde_json::Value) {
    for field in [
        "queue",
        "attachments",
        "preparations",
        "retired_blobs",
        "starter_captures",
    ] {
        stored.as_object_mut().expect("store").remove(field);
    }
    for draft in stored["drafts"].as_array_mut().expect("drafts") {
        draft.as_object_mut().expect("draft").remove("content");
    }
    for list in ["attempts", "creations"] {
        let Some(entries) = stored
            .get_mut(list)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for entry in entries {
            entry.as_object_mut().expect("entry").remove("resources");
            entry["draft"]
                .as_object_mut()
                .expect("draft")
                .remove("content");
            if entry["reservation"].is_null() {
                entry.as_object_mut().expect("entry").remove("reservation");
            }
        }
    }
}

fn context(id: &str) -> SyntheticContext {
    SyntheticContext {
        id: MessageId::from(id),
        text: format!("quoted {id} ✓"),
        description: Some("Selection".to_owned()),
        metadata: Some(Metadata(serde_json::Map::from_iter([(
            "openchamber".to_owned(),
            serde_json::json!({"kind": "quote"}),
        )]))),
    }
}

/// A steer with every part kind, after an acknowledged send in the same session.
fn multi_part() -> JournalSnapshot {
    let mut snapshot = sample();
    let (key, earlier) = &mut snapshot.attempts[0];
    earlier.admission = Admission::Admitted;
    let mut steer = attempt_tests::attempt(key);
    let id = MessageId::from("msg_bcfe56800002SteerSteer0001");
    steer.prompt.message_id = id.clone();
    steer.optimistic.info.id = id.clone();
    steer.optimistic.parts[0] = Part::text(PartId::new(format!("{id}-local-text")), "steer");
    steer.draft = Draft::new("steer");
    steer.prompt.text = "steer".to_owned();
    steer.prompt.delivery = Some(Delivery::Steer);
    steer.prompt.parts = PromptParts {
        files: vec![
            PromptFile {
                uri: "file:///home/synthetic/project/notes.md".to_owned(),
                name: Some("notes.md".to_owned()),
            },
            PromptFile {
                uri: "data:text/plain;base64,aGk=".to_owned(),
                name: None,
            },
        ],
        agents: vec![AgentMention {
            name: "plan".to_owned(),
            mention: Some(Mention {
                start: 0,
                end: 5,
                text: "@plan".to_owned(),
            }),
        }],
        skills: vec![SkillRef {
            id: "skill_fixture".to_owned(),
        }],
        metadata: Some(Metadata(serde_json::Map::from_iter([(
            "source".to_owned(),
            serde_json::json!("native"),
        )]))),
        context: vec![
            context("msg_bcfe56800001Context00001"),
            context("msg_bcfe56800001Context00002"),
        ],
    };
    let key = key.clone();
    snapshot.attempts.push((key, steer));
    snapshot
}

#[test]
fn journal_v4_round_trips_multi_part_prompts_beside_acknowledged_sends() {
    let scratch = Scratch::new();
    let snapshot = multi_part();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("open");
    journal.save(&snapshot).expect("save");
    drop(journal);
    let (_journal, value) = MutationJournal::open(&scratch.path()).expect("reopen");
    let restored = loaded(value);
    assert_eq!(restored.attempts.len(), 2);
    for ((key, expected), (restored_key, restored)) in
        snapshot.attempts.iter().zip(&restored.attempts)
    {
        assert_eq!(key, restored_key);
        assert_eq!(restored.prompt.message_id, expected.prompt.message_id);
        assert_eq!(restored.prompt.text, expected.prompt.text);
        assert_eq!(restored.prompt.parts, expected.prompt.parts);
        assert_eq!(restored.prompt.delivery, expected.prompt.delivery);
    }
    assert_eq!(restored.attempts[0].1.admission, Admission::Admitted);
    assert_eq!(
        restored.attempts[1].1.admission,
        Admission::Uncertain,
        "an interrupted steer is uncertain after restart"
    );
    let ids: Vec<_> = restored.attempts[1]
        .1
        .prompt
        .parts
        .context
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "msg_bcfe56800001Context00001",
            "msg_bcfe56800001Context00002"
        ],
        "context keeps its admission order"
    );
    let attempts = SendAttempts::from_snapshot(restored.attempts).expect("one unresolved");
    assert_eq!(attempts.session(&snapshot.attempts[0].0).count(), 2);
}

#[test]
fn journal_v3_text_snapshot_migrates_to_v4() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let (key, creation) = crate::create_attempt::tests::sample();
    let mut snapshot = sample();
    snapshot.creations.push((key, creation));
    let (mut journal, _) = MutationJournal::open(&path).expect("open");
    journal.save(&snapshot).expect("save");
    drop(journal);
    let mut stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
    as_text_only(&mut stored);
    stored["version"] = 3.into();
    let v3 = serde_json::to_vec(&stored).expect("JSON");
    write_raw(&path, &v3);
    let (mut journal, value) = MutationJournal::open(&path).expect("v3 loads");
    let migrated = loaded(value);
    assert_eq!(migrated.attempts.len(), 1);
    assert_eq!(migrated.creations.len(), 1);
    assert_eq!(migrated.drafts, snapshot.drafts);
    let attempt = &migrated.attempts[0].1;
    assert_eq!(attempt.prompt.text, snapshot.attempts[0].1.prompt.text);
    assert_eq!(
        attempt.prompt.message_id,
        snapshot.attempts[0].1.prompt.message_id
    );
    assert_eq!(attempt.prompt.delivery, Some(Delivery::Queue));
    assert!(
        attempt.prompt.parts.is_empty(),
        "a text prompt has no parts"
    );
    assert!(migrated.creations[0].1.prompt.parts.is_empty());
    journal.save(&migrated).expect("migrated save");
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
    assert_eq!(stored["version"], VERSION);
    assert_eq!(
        stored["attempts"][0]["prompt"]["context"],
        serde_json::json!([])
    );
    drop(journal);

    // A v3 file never carries v4 fields, and a v4 file never omits them.
    let mut mixed: serde_json::Value = serde_json::from_slice(&v3).expect("JSON");
    mixed["attempts"][0]["prompt"]["files"] = serde_json::json!([]);
    let mut partial = stored.clone();
    partial["attempts"][0]["prompt"]
        .as_object_mut()
        .expect("prompt")
        .remove("context");
    for malformed in [mixed, partial] {
        let bytes = serde_json::to_vec(&malformed).expect("JSON");
        write_raw(&path, &bytes);
        assert!(matches!(
            MutationJournal::open(&path),
            Err(JournalError::Invalid(_))
        ));
        assert_eq!(
            fs::read(&path).expect("kept"),
            bytes,
            "a failed load writes nothing"
        );
    }
}

#[test]
fn malformed_multi_part_snapshots_are_rejected_whole() {
    let mut cases: Vec<(&str, JournalSnapshot)> = Vec::new();
    let mut two_unresolved = multi_part();
    two_unresolved.attempts[0].1.admission = Admission::Uncertain;
    cases.push(("two unresolved attempts in one session", two_unresolved));
    let mut reused = multi_part();
    reused.attempts[1].1.prompt.parts.context[0].id =
        reused.attempts[0].1.prompt.message_id.clone();
    cases.push(("context reusing another prompt's ID", reused));
    let mut self_reference = multi_part();
    self_reference.attempts[1].1.prompt.parts.context[1].id =
        self_reference.attempts[1].1.prompt.message_id.clone();
    cases.push(("context reusing its own prompt ID", self_reference));
    let mut unprefixed = multi_part();
    unprefixed.attempts[1].1.prompt.parts.context[0].id = MessageId::from("ctx_1");
    cases.push(("context without the msg_ prefix", unprefixed));
    let mut inline = multi_part();
    inline.attempts[1].1.prompt.parts.files[1].uri =
        format!("data:image/png;base64,{}", "A".repeat(MAX_URI));
    cases.push(("an inline blob over the reference cap", inline));
    let mut metadata = multi_part();
    metadata.attempts[1].1.prompt.parts.metadata = Some(Metadata(serde_json::Map::from_iter([(
        "blob".to_owned(),
        serde_json::json!("x".repeat(MAX_METADATA)),
    )])));
    cases.push(("metadata over its cap", metadata));
    let mut blank_skill = multi_part();
    blank_skill.attempts[1].1.prompt.parts.skills[0].id = " ".to_owned();
    cases.push(("a blank skill", blank_skill));
    let (key, creation) = crate::create_attempt::tests::sample();
    let with_creation = JournalSnapshot {
        drafts: Vec::new(),
        attempts: vec![(key.clone(), attempt_tests::attempt(&key))],
        creations: vec![(key, creation)],
        selections: Vec::new(),
        ..JournalSnapshot::default()
    };
    cases.push(("a creation and an attempt for one session", with_creation));
    for (case, snapshot) in cases {
        assert!(
            matches!(Store::capture(&snapshot), Err(JournalError::Invalid(_))),
            "{case}"
        );
    }

    // The same rules hold on load, and a rejected file stays as it was.
    let scratch = Scratch::new();
    let snapshot = multi_part();
    let mut store = Store::capture(&snapshot).expect("capture");
    store.attempts[1].prompt.context[0].id = "ctx_1".to_owned();
    let bytes = serde_json::to_vec(&store).expect("encode");
    write_raw(&scratch.path(), &bytes);
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
    assert_eq!(fs::read(scratch.path()).expect("kept"), bytes);
}

fn write_raw(path: &Path, bytes: &[u8]) {
    let mut file = private_options()
        .create(true)
        .truncate(true)
        .open(path)
        .expect("write fixture");
    file.write_all(bytes).expect("write fixture bytes");
}

#[test]
fn exact_unicode_roundtrip_and_sending_recovers_uncertain() {
    let scratch = Scratch::new();
    let snapshot = sample();
    let (mut journal, initial) =
        MutationJournal::open(&scratch.path()).expect("open missing journal");
    assert!(matches!(initial, LoadedJournal::Missing));
    assert!(matches!(
        journal.save(&snapshot),
        Ok(JournalCommit::Durable)
    ));
    drop(journal);
    let (_, recovered) = MutationJournal::open(&scratch.path()).expect("reopen journal");
    let recovered = loaded(recovered);
    assert_eq!(snapshot.drafts, recovered.drafts);
    let before = &snapshot.attempts[0].1;
    let after = &recovered.attempts[0].1;
    assert_eq!(before.owner, after.owner);
    assert_eq!(before.draft, after.draft);
    assert_eq!(before.prompt.message_id, after.prompt.message_id);
    assert_eq!(before.prompt.text, after.prompt.text);
    assert_eq!(before.prompt.model, after.prompt.model);
    assert_eq!(before.prompt.agent, after.prompt.agent);
    assert_eq!(before.prompt.delivery, after.prompt.delivery);
    assert_eq!(before.optimistic, after.optimistic);
    assert_eq!(after.admission, Admission::Uncertain);
    let key = snapshot.attempts[0].0.clone();
    let mut attempts = SendAttempts::from_snapshot(recovered.attempts).expect("restore attempts");
    assert!(!attempts.can_send(&key));
    assert!(!attempts.observe(
        &attempt_tests::key("different", "session"),
        &before.prompt.message_id
    ));
    assert!(attempts.observe(&key, &before.prompt.message_id));
    assert!(attempts.can_send(&key));
}

#[test]
fn snapshot_excludes_active_aborts_and_retains_all_admission_states() {
    let mut attempts = SendAttempts::default();
    let abort = attempt_tests::key("runtime", "abort");
    assert!(attempts.begin_abort(abort.clone()));
    assert!(attempts.snapshot().is_empty());
    let restored = SendAttempts::from_snapshot(attempts.snapshot()).expect("empty recovery");
    assert!(restored.can_send(&abort));
    for state in [
        Admission::Admitted,
        Admission::Uncertain,
        Admission::Sending { observed: true },
    ] {
        let mut snapshot = sample();
        snapshot.attempts[0].1.admission = state;
        let recovered = round_trip(&snapshot);
        let expected = if matches!(state, Admission::Sending { .. }) {
            Admission::Uncertain
        } else {
            state
        };
        assert_eq!(recovered.attempts[0].1.admission, expected);
    }
}

#[test]
fn lock_is_exclusive_across_rename_and_released_on_drop() {
    let scratch = Scratch::new();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("first writer");
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Locked)
    ));
    journal.save(&sample()).expect("save");
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Locked)
    ));
    drop(journal);
    assert!(MutationJournal::open(&scratch.path()).is_ok());
}

#[test]
fn malformed_unknown_schema_and_version_fail_without_rewriting() {
    for (bytes, unsupported) in [
        (b"{broken".as_slice(), false),
        (b"{\"version\":999,\"future\":true}".as_slice(), true),
        (
            b"{\"version\":1,\"drafts\":[],\"attempts\":[],\"extra\":true}".as_slice(),
            false,
        ),
        (
            b"{\"version\":1,\"version\":1,\"drafts\":[],\"attempts\":[]}".as_slice(),
            false,
        ),
    ] {
        let scratch = Scratch::new();
        write_raw(&scratch.path(), bytes);
        let result = MutationJournal::open(&scratch.path());
        if unsupported {
            assert!(matches!(result, Err(JournalError::UnsupportedVersion(999))));
        } else {
            assert!(matches!(result, Err(JournalError::Invalid(_))));
        }
        assert_eq!(fs::read(scratch.path()).expect("existing bytes"), bytes);
    }
}

#[test]
fn invalid_snapshot_preserves_existing_target_and_selection_boundaries() {
    let scratch = Scratch::new();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("open");
    journal.save(&sample()).expect("initial write");
    let previous = fs::read(scratch.path()).expect("initial bytes");
    let mut bad = sample();
    bad.attempts[0].1.draft.selection = 1..2;
    assert!(matches!(journal.save(&bad), Err(JournalError::Invalid(_))));
    assert_eq!(fs::read(scratch.path()).expect("retained bytes"), previous);
    let snapshot = sample();
    let mut store = Store::capture(&snapshot).expect("capture");
    store.attempts[0].draft.start = 1;
    drop(journal);
    let bytes = serde_json::to_vec(&store).expect("encode malformed selection");
    write_raw(&scratch.path(), &bytes);
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
    assert_eq!(
        fs::read(scratch.path()).expect("malformed preserved"),
        bytes
    );
}

#[cfg(unix)]
#[test]
fn actual_write_failure_preserves_target() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("open");
    journal.save(&sample()).expect("initial save");
    let previous = fs::read(scratch.path()).expect("initial bytes");
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o500)).expect("disable writes");
    let result = journal.save(&sample());
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700))
        .expect("restore permissions");
    assert!(matches!(result, Err(JournalError::Io(_))));
    assert_eq!(
        fs::read(scratch.path()).expect("preserved target"),
        previous
    );
}

#[test]
fn duplicate_owners_and_ids_reject_but_runtime_namespaces_are_independent() {
    let mut snapshot = sample();
    snapshot.drafts.push(snapshot.drafts[0].clone());
    assert!(matches!(
        Store::capture(&snapshot),
        Err(JournalError::Invalid(_))
    ));
    let mut snapshot = sample();
    snapshot.attempts.push(snapshot.attempts[0].clone());
    assert!(Store::capture(&snapshot).is_err());
    let mut other = sample().attempts.remove(0);
    other.0.runtime = RuntimeId::from("isolated-runtime");
    other.1.owner = DraftKey::new(
        other.0.runtime.clone(),
        "/project",
        DraftTarget::Session(other.0.session.clone()),
    )
    .expect("owner");
    let mut snapshot = sample();
    snapshot.attempts.push(other);
    assert!(Store::capture(&snapshot).is_ok());
    snapshot.attempts[1].0.runtime = RuntimeId::from("wrong-runtime");
    assert!(Store::capture(&snapshot).is_err());
}

#[test]
fn duplicate_message_ids_and_malformed_identifiers_reject_on_load() {
    let scratch = Scratch::new();
    let mut snapshot = sample();
    let mut other = sample().attempts.remove(0);
    other.0.session = SessionId::from("second-session");
    other.1.owner = DraftKey::new(
        other.0.runtime.clone(),
        "/project",
        DraftTarget::Session(other.0.session.clone()),
    )
    .expect("owner");
    snapshot.attempts.push(other);
    assert!(
        Store::capture(&snapshot).is_err(),
        "message IDs must be unique within a runtime"
    );

    let snapshot = sample();
    let mut store = Store::capture(&snapshot).expect("capture");
    store.attempts[0].prompt.message_id.clear();
    let bytes = serde_json::to_vec(&store).expect("encode malformed ID");
    write_raw(&scratch.path(), &bytes);
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
    assert_eq!(
        fs::read(scratch.path()).expect("retained malformed ID"),
        bytes
    );
}

#[cfg(unix)]
#[test]
fn unreadable_target_is_io_failure_and_remains_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let bytes = b"{\"version\":1,\"drafts\":[],\"attempts\":[]}";
    write_raw(&scratch.path(), bytes);
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o000)).expect("deny read");
    let result = MutationJournal::open(&scratch.path());
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o600)).expect("restore read");
    assert!(matches!(result, Err(JournalError::Io(_))));
    assert_eq!(
        fs::read(scratch.path()).expect("retained unreadable file"),
        bytes
    );
}

#[cfg(target_os = "linux")]
#[test]
fn directory_sync_failure_reports_that_rename_already_committed() {
    let scratch = Scratch::new();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("open");
    journal
        .save(&JournalSnapshot::default())
        .expect("initial save");
    // procfs rejects fsync, exercising the post-rename uncertainty branch.
    journal.parent = File::open("/proc/version").expect("read-only procfs fixture");
    assert!(matches!(
        journal.save(&sample()),
        Ok(JournalCommit::DirectorySyncUncertain(_))
    ));
    drop(journal);
    let (_, recovered) = MutationJournal::open(&scratch.path()).expect("read committed target");
    assert_eq!(loaded(recovered).attempts.len(), 1);
}

#[test]
fn empty_success_is_distinct_and_limits_reject_without_truncation() {
    let scratch = Scratch::new();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("open");
    journal
        .save(&JournalSnapshot::default())
        .expect("persist empty");
    drop(journal);
    let (_, recovered) = MutationJournal::open(&scratch.path()).expect("load empty");
    let snapshot = loaded(recovered);
    assert!(snapshot.drafts.is_empty() && snapshot.attempts.is_empty());
    let mut oversized = sample();
    oversized.drafts[0].1 = Draft::new("a".repeat(MAX_TEXT + 1));
    assert!(Store::capture(&oversized).is_err());
    let file = private_options()
        .truncate(true)
        .open(scratch.path())
        .expect("oversized file");
    file.set_len(MAX_FILE + 1)
        .expect("sparse oversized fixture");
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
}

#[cfg(unix)]
#[test]
fn unix_permissions_and_nonregular_files_fail_closed() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let scratch = Scratch::new();
    let (mut journal, _) = MutationJournal::open(&scratch.path()).expect("open");
    journal.save(&sample()).expect("save");
    assert_eq!(
        fs::metadata(&scratch.0).expect("directory mode").mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(scratch.path()).expect("file mode").mode() & 0o777,
        0o600
    );
    drop(journal);
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o644))
        .expect("unsafe permission fixture");
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
    fs::remove_file(scratch.path()).expect("remove fixture");
    symlink("missing", scratch.path()).expect("symlink fixture");
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
    fs::remove_file(scratch.path()).expect("remove symlink");
    fs::create_dir(scratch.path()).expect("nonregular fixture");
    assert!(matches!(
        MutationJournal::open(&scratch.path()),
        Err(JournalError::Invalid(_))
    ));
}
