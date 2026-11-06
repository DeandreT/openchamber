use super::*;

use openchamber_native_core::draft::Draft;
use openchamber_native_core::draft_content::{
    AgentMention, DraftContext, DraftFile, DraftFileContent, Mention, SkillRef,
};
use openchamber_native_core::ids::ContextItemId;
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::json;

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Seal caller-measured text/context metadata against the actual uploaded bytes.
/// This constructs no Take, acknowledgement or attachment file proof.
pub(crate) fn indexed_fixture(
    bytes: Vec<u8>,
    directory: &str,
    chunks: Vec<openchamber_native_transport::queue_manifest::QueueAttachmentDescriptor>,
    index: openchamber_native_transport::queue_manifest::QueuePayloadIndex,
    usage: QueuePayloadUse,
) -> (QueueIndexedPayload, QueuePayloadDocument) {
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Chunk<'a> {
        attachment_id: &'a str,
        filename: &'a str,
        mime_type: &'a str,
        size: u64,
        sha256: String,
        blob_id: String,
        request_id: &'a str,
        ordinal: usize,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Manifest<'a> {
        version: u32,
        encoding: &'a str,
        total_bytes: usize,
        sha256: String,
        chunks: Vec<Chunk<'a>>,
    }
    #[derive(Serialize)]
    struct Config<'a> {
        #[serde(rename = "providerID")]
        provider: &'a str,
        #[serde(rename = "modelID")]
        model: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        variant: Option<&'a str>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Index<'a> {
        directory: &'a str,
        send_config: Config<'a>,
        preview: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_preview: Option<&'a str>,
        entry_count: usize,
        context_ids: Vec<&'a str>,
        attachment_blob_ids: Vec<String>,
        prompt_bytes: u64,
        admission_bytes: u64,
        structured_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        legacy_attachment_ids: Option<&'a Vec<Option<String>>>,
    }
    let document = QueuePayloadDocument::from_bytes(bytes, directory, usage, sha256)
        .expect("actual byte-backed fixture payload");
    match document.item() {
        QueuePayloadItem::Manifest(item) => assert!(
            item.attachment_manifest.is_empty()
                && item.prompt_parts.file_references.is_empty()
                && item.prompt_parts.agents.is_empty()
                && item.prompt_parts.skills.is_empty(),
            "this fixture measures text/context only"
        ),
        QueuePayloadItem::Legacy(item) => assert!(
            item.attachments.is_empty(),
            "rich files require their own measured fixture"
        ),
    }
    assert!(
        index.attachment_blob_ids.is_empty()
            && index
                .legacy_attachment_ids
                .as_ref()
                .is_none_or(Vec::is_empty),
        "text/context metadata cannot invent file roots"
    );
    let pointer = document
        .pointer(chunks.clone(), sha256)
        .expect("actual uploaded descriptors bind the original bytes");
    let manifest = Manifest {
        version: 1,
        encoding: "utf-8-json",
        total_bytes: document.bytes().len(),
        sha256: hex(&document.sha256()),
        chunks: chunks
            .iter()
            .enumerate()
            .map(|(ordinal, chunk)| Chunk {
                attachment_id: &chunk.attachment_id,
                filename: &chunk.filename,
                mime_type: &chunk.mime_type,
                size: chunk.size,
                sha256: hex(&chunk.sha256),
                blob_id: hex(&chunk.blob_id),
                request_id: &chunk.request_id,
                ordinal,
            })
            .collect(),
    };
    let wire = Index {
        directory: &index.directory,
        send_config: Config {
            provider: &index.send_config.provider_id,
            model: &index.send_config.model_id,
            agent: index.send_config.agent.as_deref(),
            variant: index.send_config.variant.as_deref(),
        },
        preview: &index.preview,
        context_preview: index.context_preview.as_deref(),
        entry_count: index.entry_count,
        context_ids: index.context_ids.iter().map(|id| id.as_str()).collect(),
        attachment_blob_ids: Vec::new(),
        prompt_bytes: index.prompt_bytes,
        admission_bytes: index.admission_bytes,
        structured_bytes: index.structured_bytes,
        legacy_attachment_ids: index.legacy_attachment_ids.as_ref(),
    };
    let manifest = serde_json::to_string(&manifest).expect("backend manifest schema order");
    let wire = serde_json::to_string(&wire).expect("backend index schema order");
    let binding = format!(r#"{{"payloadManifest":{manifest},"payloadIndex":{wire}}}"#);
    let seal = hex(&sha256(binding.as_bytes()));
    let body = format!(
        r#"{{"format":"payload-v1","payloadManifest":{manifest},"payloadIndex":{wire},"payloadIndexHash":"{seal}"}}"#
    );
    let indexed = QueueIndexedPayload::from_body(body.as_bytes(), directory, sha256)
        .expect("strict original server-index schema and binding hash");
    assert_eq!(
        indexed.pointer(),
        &pointer,
        "sealing cannot change the actual uploaded pointer"
    );
    let original = document.bytes().to_vec();
    let materialized = indexed
        .materialize(original.clone(), usage, sha256)
        .expect("strict byte and measured-index materialization");
    assert_eq!(
        materialized.bytes(),
        original,
        "sealing preserves every original byte"
    );
    (indexed, materialized)
}

fn owner() -> DraftKey {
    DraftKey::new(
        RuntimeId::from("original-host"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_queue")),
    )
    .expect("captured owner")
}

fn native() -> QueueNativeSource {
    let mut draft = Draft::new("@build existing λ");
    draft.content.agents.push(AgentMention {
        name: "build".into(),
        mention: Some(Mention {
            start: 0,
            end: 6,
            text: "@build".into(),
        }),
    });
    draft.content.skills.push(SkillRef {
        id: "native_skill".into(),
    });
    draft.content.metadata_json = Some(r#"{"source":"native"}"#.into());
    draft.content.context.push(DraftContext {
        id: ContextItemId::from("native-chip"),
        text: "native context".into(),
        description: Some("native description".into()),
        metadata_json: None,
        instructions: Some("native instructions".into()),
    });
    // Source-document group identity remains native even when the queue item has
    // an unrelated file with the same display name.
    draft.content.files.push(DraftFile {
        id: AttachmentId::from("native-reference"),
        name: Some("notes.txt".into()),
        media_type: None,
        byte_len: None,
        source_document_id: Some(AttachmentId::from("native-group")),
        content: DraftFileContent::Reference {
            uri: "file:///project/native.txt".into(),
        },
    });
    QueueNativeSource {
        owner: owner(),
        draft,
    }
}

fn item() -> MaterializedQueueItem {
    let item = json!({"format":"manifest-v1","content":"queued display","text":"queued delivered",
        "sendConfig":{"providerID":"provider","modelID":"model","agent":"build","variant":"high"},
        "attachmentManifest":[{"blobId":"ab".repeat(32),"requestId":"original_file_request","attachmentId":"file-0",
            "filename":"notes.txt","mimeType":"text/plain","size":8,"sha256":hex(&sha256(b"original")),
            "description":"file description","mention":{"start":1,"end":6,"text":"@file"}}],
        "promptParts":{"agents":[{"name":"build","mention":{"start":2,"end":8,"text":"@build"}}],
            "skills":[{"id":"queued_skill","mention":{"start":3,"end":9,"text":"$skill"}}],
            "metadata":{"source":"queued","nested":{"keep":[1,true,"λ"]}},
            "context":[{"id":"msg_original_context","text":"queued context","description":"queued description","metadata":{"keep":true}}]}});
    let bytes =
        serde_json::to_vec(&json!({"directory":"/project","item":item})).expect("payload JSON");
    let document =
        QueuePayloadDocument::from_bytes(bytes, "/project", QueuePayloadUse::Enqueue, sha256)
            .expect("typed rich payload");
    let size = document.bytes().len();
    let digest = hex(&document.sha256());
    let pointer = format!(
        r#"{{"version":1,"encoding":"utf-8-json","totalBytes":{size},"sha256":"{digest}","chunks":[{{"attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":{size},"sha256":"{digest}","blobId":"{}","requestId":"original_payload_request","ordinal":0}}]}}"#,
        "cd".repeat(32)
    );
    let index = format!(
        r#"{{"directory":"/project","sendConfig":{{"providerID":"provider","modelID":"model","agent":"build","variant":"high"}},"preview":"queued display","entryCount":4,"contextIds":["msg_original_context"],"attachmentBlobIds":["{}"],"promptBytes":128,"admissionBytes":256,"structuredBytes":0}}"#,
        "ab".repeat(32)
    );
    let binding = format!(r#"{{"payloadManifest":{pointer},"payloadIndex":{index}}}"#);
    let seal = hex(&sha256(binding.as_bytes()));
    let body = format!(
        r#"{{"format":"payload-v1","payloadManifest":{pointer},"payloadIndex":{index},"payloadIndexHash":"{seal}"}}"#
    );
    let indexed = QueueIndexedPayload::from_body(body.as_bytes(), "/project", sha256)
        .expect("original sealed index");
    let document = indexed
        .materialize(document.bytes().to_vec(), QueuePayloadUse::Enqueue, sha256)
        .expect("sealed materialization");
    MaterializedQueueItem::from_materialized(
        QueueTransferItem {
            item: "q_original".into(),
            index: QueueJson::new(indexed.body().expect("original index body"))
                .expect("bounded index"),
            payload: QueueObjectId::new("original-payload").expect("payload object"),
            files: vec![QueueFileBinding::Manifest {
                blob: [0xab; 32],
                local: format!("{:032x}", 100),
                byte_len: 8,
                sha256: sha256(b"original"),
            }],
        },
        indexed,
        document,
    )
    .expect("original sealed materialization and file proof")
}

#[test]
fn rich_edit_append_keeps_native_content_and_every_original_queue_field() {
    let native = native();
    let before = native.draft.clone();
    let item = item();
    assert_eq!(
        crate::draft_companion::display_text(&native.draft, std::slice::from_ref(&item)),
        "@build existing λ\n\nqueued display"
    );
    assert_eq!(
        native.draft, before,
        "the companion does not overwrite native source groups, metadata or typing"
    );
    let document = CompanionDocument::new(native.clone(), vec![item.retained.clone()]);
    let restored: CompanionDocument =
        serde_json::from_slice(&serde_json::to_vec(&document).expect("companion spool bytes"))
            .expect("complete companion after restart");
    assert_eq!(restored.native.draft, before);
    assert_eq!(
        restored.items[0].index.bytes(),
        item.indexed.body().expect("exact sealed bytes")
    );
    let QueuePayloadItem::Manifest(rich) = item.document.item() else {
        panic!("manifest payload");
    };
    assert_eq!(rich.content, "queued display");
    assert_eq!(rich.text, "queued delivered");
    assert_eq!(
        rich.prompt_parts.context[0]
            .id
            .as_ref()
            .expect("original context ID")
            .as_str(),
        "msg_original_context"
    );
    assert_eq!(
        rich.prompt_parts.skills[0]
            .mention
            .as_ref()
            .expect("skill mention")
            .text,
        "$skill"
    );
    assert_eq!(
        rich.attachment_manifest[0].description.as_deref(),
        Some("file description")
    );
    assert_eq!(
        rich.attachment_manifest[0]
            .mention
            .as_ref()
            .expect("file mention")
            .start,
        1
    );
    assert_eq!(
        rich.prompt_parts
            .metadata
            .as_ref()
            .expect("original metadata")
            .0["source"],
        "queued"
    );
    assert!(validate_file_bindings(&item).is_ok());
}

#[test]
fn original_materialized_seals_reject_same_length_wrong_files_and_fresh_parsing() {
    let mut original = item();
    assert!(validate_file_bindings(&original).is_ok());
    original.retained.files[0] = QueueFileBinding::Manifest {
        blob: [0xab; 32],
        local: format!("{:032x}", 100),
        byte_len: 8,
        sha256: sha256(b"tampered"),
    };
    assert!(
        validate_file_bindings(&original).is_err(),
        "a matching length grants no original-byte authority"
    );
    let parsed = QueuePayloadDocument::from_bytes(
        original.document.bytes().to_vec(),
        "/project",
        QueuePayloadUse::Enqueue,
        sha256,
    )
    .expect("freshly parsed payload");
    assert!(
        MaterializedQueueItem::from_materialized(original.retained, original.indexed, parsed)
            .is_err(),
        "fresh parse has no materialized index seal"
    );
}

#[test]
fn queued_send_frame_keeps_original_configuration_context_ids_and_delivered_text() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("owned test runtime");
    let client = ServerClient::new(
        Endpoint::parse("http://localhost:48123/base").expect("captured endpoint"),
        openchamber_native_transport::credential::Credential::None,
    )
    .expect("client");
    let server = QueueServer::new(
        RuntimeId::from("original-host"),
        client,
        executor.handle().clone(),
        SessionId::from("ses_queue"),
        "/project",
    )
    .expect("captured server");
    let item = item();
    let files = vec![vec![
        QueuePreparedFile::new(
            QueuePayloadFileSource::Manifest([0xab; 32]),
            8,
            sha256(b"original"),
            "file:///tmp/openchamber/0123456789abcdef/queue.bin",
        )
        .expect("caller binding"),
    ]];
    let frozen = freeze_send(
        &server,
        &MessageId::from("msg_original_prompt"),
        std::slice::from_ref(&item),
        &files,
        0,
        Some(Delivery::Steer),
    )
    .expect("full lossless frame");
    let body: serde_json::Value =
        serde_json::from_slice(frozen.prompt_body()).expect("prompt JSON");
    assert_eq!(
        body["text"], "queued delivered",
        "display text never replaces delivered text"
    );
    assert_eq!(body["skills"][0]["mention"]["text"], "$skill");
    assert_eq!(body["files"][0]["description"], "file description");
    assert_eq!(
        frozen.context_ids(),
        [MessageId::from("msg_original_context")]
    );
    let digest = frozen.sha256();
    let config = frozen.configuration();
    assert_eq!(config.variant.as_deref(), Some("high"));
    let bytes = frozen.into_bytes();
    let restored = QueueFrozenSend::from_bytes(
        &server.client,
        QueueFrozenSendOwner {
            session: server.scope.session(),
            directory: server.scope.directory(),
            message_id: &MessageId::from("msg_original_prompt"),
            configuration: &config,
        },
        bytes,
        digest,
        sha256,
    )
    .expect("exact original frame after restart");
    assert_eq!(restored.message_id().as_str(), "msg_original_prompt");
    assert_eq!(
        restored.context_ids(),
        [MessageId::from("msg_original_context")]
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(restored.prompt_body()).expect("restored JSON"),
        body
    );
}

#[test]
fn take_tasks_hold_the_original_host_through_disk_waits_without_blocking_other_owners() {
    let mut state = TakeState::default();
    let id = QueueOperationId::new("original_take_request").expect("stable request ID");
    state.tasks.insert(
        id.clone(),
        TakeTask {
            runtime: RuntimeId::from("original-host"),
            _task: Task::ready(()),
        },
    );
    assert!(state.holds_runtime(&RuntimeId::from("original-host")));
    assert!(!state.holds_runtime(&RuntimeId::from("replacement-host")));
    assert!(!state.busy(&QueueOperationId::new("independent_take").expect("independent request")));
    state.tasks.remove(&id);
    assert!(!state.any_busy());
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
    use crate::queue_capture::QueueCaptureBytes;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Profile(PathBuf);
    impl Profile {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = PathBuf::from(format!(
                "/tmp/opencode/queue-take-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("test-owned private profile");
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("journal.json")
        }
    }
    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
        assert!(
            matches!(journal.save(snapshot), Ok(JournalCommit::Durable)),
            "complete snapshot is durable"
        );
    }

    fn capture(
        journal: &mut AttachmentJournal,
        snapshot: &mut JournalSnapshot,
        name: &str,
        bytes: Vec<u8>,
        kind: QueueObjectKind,
        label: u128,
    ) -> QueueObjectId {
        let blob = BlobId::parse(&format!("{label:032x}")).expect("local chunk label");
        let capture = QueueCaptureBytes::new(kind, bytes, &[blob]).expect("small test capture");
        let id = QueueObjectId::new(name).expect("local object identity");
        let object = QueueObject::planned(
            id.clone(),
            snapshot.queue.operations()[0].id().clone(),
            kind,
            1,
            capture.reference().clone(),
        )
        .expect("owned capture");
        assert!(snapshot.queue.admit_object(object, |_| Ok(())).is_ok());
        save(journal, snapshot);
        let hold = snapshot
            .queue
            .object(&id)
            .expect("object")
            .hold(0)
            .expect("chunk hold");
        let receipt = journal
            .put_queue(&hold, capture.bytes())
            .expect("exact held bytes");
        snapshot
            .queue
            .record_chunk(&hold, &receipt)
            .expect("durable put receipt");
        save(journal, snapshot);
        let proof = journal
            .verify_queue(&snapshot.queue.object(&id).expect("object").verification())
            .expect("worker whole-stream verification");
        snapshot
            .queue
            .record_verified(&proof)
            .expect("retain verified capture");
        save(journal, snapshot);
        id
    }

    fn json(value: serde_json::Value) -> QueueJson {
        QueueJson::new(serde_json::to_vec(&value).expect("JSON bytes")).expect("compact metadata")
    }

    fn setup(
        journal: &mut AttachmentJournal,
    ) -> (JournalSnapshot, QueueOperationId, MaterializedQueueItem) {
        setup_for(journal, QueueTakePurpose::Edit)
    }

    fn send_frame(
        item: &MaterializedQueueItem,
        message: &str,
        delivery: Option<Delivery>,
    ) -> Vec<u8> {
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("owned inert test runtime");
        let client = ServerClient::new(
            Endpoint::parse("http://localhost:48123/base").expect("captured test endpoint"),
            openchamber_native_transport::credential::Credential::None,
        )
        .expect("test client");
        let server = QueueServer::new(
            RuntimeId::from("original-host"),
            client,
            executor.handle().clone(),
            SessionId::from("ses_queue"),
            "/project",
        )
        .expect("exact owner");
        let files = vec![vec![
            QueuePreparedFile::new(
                QueuePayloadFileSource::Manifest([0xab; 32]),
                8,
                sha256(b"original"),
                "file:///tmp/openchamber/0123456789abcdef/queue.bin",
            )
            .expect("bound fixture file"),
        ]];
        freeze_send(
            &server,
            &MessageId::from(message),
            std::slice::from_ref(item),
            &files,
            0,
            delivery,
        )
        .expect("original frozen frame")
        .into_bytes()
    }

    fn setup_for(
        journal: &mut AttachmentJournal,
        purpose: QueueTakePurpose,
    ) -> (JournalSnapshot, QueueOperationId, MaterializedQueueItem) {
        let mut item = item();
        let id = QueueOperationId::new("original_take_request").expect("stable identity");
        let owner = QueueOwner::new(
            &RuntimeId::from("original-host"),
            "/project",
            &SessionId::from("ses_queue"),
            &owner(),
        )
        .expect("captured owner");
        let body =
            json(json!({"directory":"/project","format":"payload-v1","itemIds":["q_original"]}));
        let operation = QueueOperation::take(
            id.clone(),
            owner,
            QueueConfiguration::capture(item.document.item().send_config()),
            purpose.clone(),
            body,
        );
        let mut snapshot = JournalSnapshot::default();
        assert!(
            snapshot
                .queue
                .admit_operation(operation, |_| Ok(()))
                .is_ok()
        );
        snapshot.drafts.push((native().owner, native().draft));
        let descriptor = item.retained.files[0]
            .descriptor()
            .expect("original local descriptor");
        let hold = PreparationHold {
            owner: super::owner(),
            id: AttachmentId::from(descriptor.id().as_str()),
            generation: 1,
            blob: *descriptor.id(),
        };
        snapshot.preparations.push(hold.clone());
        save(journal, &snapshot);
        let receipt = journal
            .put(&hold, b"original")
            .expect("original attachment bytes");
        snapshot.preparations.clear();
        snapshot.attachments.insert(
            hold.id,
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: None,
            },
        );
        item.retained.payload = capture(
            journal,
            &mut snapshot,
            "original-payload",
            item.document.bytes().to_vec(),
            QueueObjectKind::Payload,
            101,
        );
        let transfer = QueueTransfer {
            receipt: json(
                json!({"receipt":{"version":1,"kind":"take","requestId":"original_take_request",
            "sessionId":"ses_queue","directory":"/project","requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["q_original"]},
            "state":"transferring","revision":1}),
            ),
            items: vec![item.retained.clone()],
        };
        snapshot
            .queue
            .record_take_transfer(&id, transfer, &snapshot.attachments, |_| Ok(()))
            .expect("retain original transfer");
        let native_source = match &purpose {
            QueueTakePurpose::Edit => native(),
            QueueTakePurpose::Send { .. } => QueueNativeSource {
                owner: super::owner(),
                draft: Draft::default(),
            },
        };
        let companion = CompanionDocument::new(native_source.clone(), vec![item.retained.clone()]);
        let companion = capture(
            journal,
            &mut snapshot,
            "original-companion",
            serde_json::to_vec(&companion).expect("companion bytes"),
            QueueObjectKind::NativeCompanion,
            102,
        );
        snapshot
            .queue
            .retain_take_native_source(&id, &native_source, |_| Ok(()))
            .expect("native source group roots");
        let destination = match purpose {
            QueueTakePurpose::Edit => QueueDestination::Draft { companion },
            QueueTakePurpose::Send { message, delivery } => {
                let frozen = send_frame(&item, &message, delivery);
                let prompt = capture(
                    journal,
                    &mut snapshot,
                    "original-send",
                    frozen,
                    QueueObjectKind::FrozenSend,
                    103,
                );
                QueueDestination::Send {
                    companion,
                    prompt,
                    message,
                }
            }
        };
        snapshot
            .queue
            .prepare_take_destination(
                &id,
                destination,
                json(json!({"directory":"/project"})),
                &snapshot.attachments,
                |_| Ok(()),
            )
            .expect("atomic destination and first-class composer owner");
        JournalView::from(&snapshot)
            .validate()
            .expect("complete canonical snapshot");
        (snapshot, id, item)
    }

    fn admitted_send(
        journal: &mut AttachmentJournal,
    ) -> (JournalSnapshot, QueueOperationId, MaterializedQueueItem) {
        let (mut snapshot, id, item) = setup_for(
            journal,
            QueueTakePurpose::Send {
                message: "msg_original_prompt".into(),
                delivery: Some(Delivery::Steer),
            },
        );
        save(journal, &snapshot);
        let proof = item
            .document
            .files_proof(sha256)
            .expect("sealed original files");
        let permit = journal
            .authorize_queue_acknowledgement(&id, &[proof])
            .expect("durable complete destination");
        let observed = json(
            json!({"receipt":{"version":1,"kind":"take","requestId":"original_take_request",
            "sessionId":"ses_queue","directory":"/project","requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["q_original"]},
            "state":"acknowledged","revision":2}),
        );
        snapshot
            .queue
            .record_take_acknowledged(&permit, observed, |_| Ok(()))
            .expect("acknowledged fixture");
        snapshot
            .queue
            .record_take_send_state(&id, QueueTakeSendState::Admitted, |_| Ok(()))
            .expect("authoritative admission fixture");
        save(journal, &snapshot);
        (snapshot, id, item)
    }

    fn blob_path(profile: &Profile, id: &BlobId) -> PathBuf {
        profile
            .0
            .join("attachment-spool")
            .join(format!("blob-{}.bin", id.as_str()))
    }

    fn partial(profile: &Profile, id: &BlobId, bytes: &[u8]) -> u64 {
        let path = blob_path(profile, id);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("private short-file artifact");
        file.write_all(&bytes[..bytes.len().min(3)])
            .expect("surviving short write");
        file.sync_all().expect("persist short-file fixture");
        fs::metadata(path).expect("short file inode").ino()
    }

    fn pending_download(
        journal: &mut AttachmentJournal,
    ) -> (
        JournalSnapshot,
        QueueOperationId,
        MaterializedQueueItem,
        PreparationHold,
    ) {
        let original = item();
        let id = QueueOperationId::new("original_take_request").expect("stable take request");
        let owner = QueueOwner::new(
            &RuntimeId::from("original-host"),
            "/project",
            &SessionId::from("ses_queue"),
            &owner(),
        )
        .expect("exact owner");
        let request =
            json(json!({"directory":"/project","format":"payload-v1","itemIds":["q_original"]}));
        let mut snapshot = JournalSnapshot::default();
        snapshot.drafts.push((native().owner, native().draft));
        assert!(
            snapshot
                .queue
                .admit_operation(
                    QueueOperation::take(
                        id.clone(),
                        owner,
                        QueueConfiguration::capture(original.document.item().send_config()),
                        QueueTakePurpose::Send {
                            message: "msg_original_prompt".into(),
                            delivery: Some(Delivery::Steer)
                        },
                        request
                    ),
                    |_| Ok(())
                )
                .is_ok()
        );
        let descriptor = original.retained.files[0]
            .descriptor()
            .expect("sealed original local binding");
        let hold = PreparationHold {
            owner: super::owner(),
            id: AttachmentId::from(descriptor.id().as_str()),
            generation: 1,
            blob: *descriptor.id(),
        };
        snapshot.preparations.push(hold.clone());
        let payload = QueueCaptureBytes::new(
            QueueObjectKind::Payload,
            original.document.bytes().to_vec(),
            &[BlobId::parse(&format!("{:032x}", 101)).expect("original local payload label")],
        )
        .expect("original payload capture");
        assert!(
            snapshot
                .queue
                .admit_object(
                    QueueObject::planned(
                        original.retained.payload.clone(),
                        id.clone(),
                        QueueObjectKind::Payload,
                        1,
                        payload.reference().clone()
                    )
                    .expect("original payload hold"),
                    |_| Ok(())
                )
                .is_ok()
        );
        let transfer = QueueTransfer {
            receipt: json(
                json!({"receipt":{"version":1,"kind":"take","requestId":"original_take_request",
            "sessionId":"ses_queue","directory":"/project","requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["q_original"]},
            "state":"transferring","revision":1}),
            ),
            items: vec![original.retained.clone()],
        };
        snapshot
            .queue
            .record_take_transfer(&id, transfer, &snapshot.attachments, |_| Ok(()))
            .expect("retain original binding before download");
        save(journal, &snapshot);
        (snapshot, id, original, hold)
    }

    #[test]
    fn explicit_take_retry_resumes_downloaded_attachment_only_under_its_original_committed_descriptor()
     {
        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("private journal");
        let (snapshot, id, original, hold) = pending_download(&mut journal);
        let descriptor = original.retained.files[0]
            .descriptor()
            .expect("original seal binding");
        drop(journal);
        let inode = partial(&profile, descriptor.id(), b"original");
        let (worker, loaded) = JournalWorker::start(profile.path());
        let LoadedJournal::Present(mut restored) = loaded
            .blocking_recv()
            .expect("restart answers")
            .expect("restart loads")
        else {
            panic!("retained transfer and hold");
        };
        assert_eq!(
            Activation::Initial
                .write_attachment(&worker, hold.clone(), descriptor, b"original".to_vec())
                .blocking_recv()
                .expect("initial put answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Collision
            ))),
            "initial take cannot repair a pre-existing short file"
        );
        let foreign = BlobDescriptor::new(*descriptor.id(), 8, sha256(b"original"));
        let mut foreign_hold = hold.clone();
        foreign_hold.owner = DraftKey::new(
            RuntimeId::from("foreign-host"),
            "/project",
            DraftTarget::Session(SessionId::from("ses_queue")),
        )
        .expect("foreign owner");
        assert_eq!(
            Activation::Retry
                .write_attachment(&worker, foreign_hold, foreign, b"original".to_vec())
                .blocking_recv()
                .expect("foreign retry answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        let wrong = BlobDescriptor::new(*descriptor.id(), 8, sha256(b"originaX"));
        assert_eq!(
            Activation::Retry
                .write_attachment(&worker, hold.clone(), wrong, b"originaX".to_vec())
                .blocking_recv()
                .expect("changed descriptor answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized)),
            "a matching prefix cannot change the committed full-byte descriptor"
        );
        let mut refused = JournalSnapshot {
            drafts: snapshot.drafts.clone(),
            attachments: snapshot.attachments.clone(),
            preparations: snapshot.preparations.clone(),
            queue: snapshot.queue.clone(),
            ..JournalSnapshot::default()
        };
        refused.drafts[0].1.content.metadata_json = Some("[]".into());
        assert!(
            worker
                .submit(refused)
                .blocking_recv()
                .expect("failed save answers")
                .is_err()
        );
        assert_eq!(
            Activation::Retry
                .write_attachment(&worker, hold.clone(), descriptor, b"original".to_vec())
                .blocking_recv()
                .expect("revoked retry answers"),
            Err(BlobFailure::Attachment(
                AttachmentFailure::NoDurableAuthority
            ))
        );
        assert_eq!(
            worker
                .submit(snapshot)
                .blocking_recv()
                .expect("fresh authority save answers"),
            Ok(CommitOutcome::Durable)
        );
        let receipt = Activation::Retry
            .write_attachment(&worker, hold.clone(), descriptor, b"original".to_vec())
            .blocking_recv()
            .expect("explicit retry answers")
            .expect("exact bound prefix resumes");
        assert_eq!(receipt.descriptor, descriptor);
        assert_eq!(receipt.commit, SpoolCommit::Durable);
        assert_eq!(
            fs::metadata(blob_path(&profile, descriptor.id()))
                .expect("same local file")
                .ino(),
            inode
        );
        assert_eq!(
            fs::read(blob_path(&profile, descriptor.id())).expect("complete original bytes"),
            b"original"
        );
        assert_eq!(
            restored.queue.operation(&id).expect("stable request").id(),
            &id
        );
        let QueueAction::Take {
            phase: QueueTakePhase::Transferring(transfer),
            ..
        } = restored
            .queue
            .operation(&id)
            .expect("original transfer")
            .action()
        else {
            panic!("retained transfer");
        };
        assert_eq!(
            transfer.items[0].index, original.retained.index,
            "repair retains the original capture/index seal"
        );
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
                .expect("publication answers"),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(
            Activation::Retry
                .write_attachment(&worker, hold, descriptor, b"original".to_vec())
                .blocking_recv()
                .expect("published retry answers"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized)),
            "catalog publication revokes unpublished repair authority"
        );
        assert_eq!(
            worker.finish().blocking_recv().expect("drain answers"),
            Ok(CommitOutcome::Durable)
        );
    }

    #[test]
    fn explicit_take_retry_resumes_original_payload_destination_and_frame_chunk_labels_after_restart()
     {
        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("private journal");
        let (mut snapshot, id, original, _) = pending_download(&mut journal);
        let companion = serde_json::to_vec(&CompanionDocument::new(
            native(),
            vec![original.retained.clone()],
        ))
        .expect("original companion bytes");
        let mut captures = vec![(
            original.retained.payload.clone(),
            QueueObjectKind::Payload,
            original.document.bytes().to_vec(),
        )];
        for (name, kind, bytes, label) in [
            (
                "retry-destination",
                QueueObjectKind::NativeCompanion,
                companion,
                102,
            ),
            (
                "retry-frame",
                QueueObjectKind::FrozenSend,
                send_frame(&original, "msg_original_prompt", Some(Delivery::Steer)),
                103,
            ),
        ] {
            let object = QueueObjectId::new(name).expect("original local object");
            let capture = QueueCaptureBytes::new(
                kind,
                bytes.clone(),
                &[BlobId::parse(&format!("{label:032x}")).expect("local-only label")],
            )
            .expect("original held bytes");
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
                        .expect("owned object"),
                        |_| Ok(())
                    )
                    .is_ok()
            );
            captures.push((object, kind, bytes));
        }
        save(&mut journal, &snapshot);
        drop(journal);
        let mut held = Vec::new();
        for (object, _, bytes) in &captures {
            let hold = snapshot
                .queue
                .object(object)
                .expect("retained original object")
                .hold(0)
                .expect("unpublished chunk");
            let inode = partial(
                &profile,
                hold.chunk.descriptor().expect("exact local label").id(),
                bytes,
            );
            held.push((hold, inode));
        }
        let (worker, loaded) = JournalWorker::start(profile.path());
        let LoadedJournal::Present(mut restored) = loaded
            .blocking_recv()
            .expect("restart answers")
            .expect("restart loads")
        else {
            panic!("all original holds");
        };
        for ((object, _, bytes), (hold, inode)) in captures.iter().zip(held) {
            assert_eq!(
                Activation::Initial
                    .write_chunk(&worker, hold.clone(), bytes.clone())
                    .blocking_recv()
                    .expect("initial write answers"),
                Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                    SpoolError::Collision
                )))
            );
            let receipt = Activation::Retry
                .write_chunk(&worker, hold.clone(), bytes.clone())
                .blocking_recv()
                .expect("explicit retry answers")
                .expect("original chunk resumes");
            assert_eq!(
                fs::metadata(blob_path(&profile, receipt.descriptor.id()))
                    .expect("unchanged inode")
                    .ino(),
                inode
            );
            assert_eq!(
                fs::read(blob_path(&profile, receipt.descriptor.id()))
                    .expect("whole captured bytes"),
                *bytes
            );
            restored
                .queue
                .record_chunk(&hold, &receipt)
                .expect("same durable chunk receipt");
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
                        .object(object)
                        .expect("same object")
                        .verification(),
                )
                .blocking_recv()
                .expect("verification answers")
                .expect("whole capture matches");
            restored
                .queue
                .record_verified(&proof)
                .expect("publish verified capture");
            assert_eq!(
                worker
                    .submit((*restored).clone())
                    .blocking_recv()
                    .expect("verification save answers"),
                Ok(CommitOutcome::Durable)
            );
            assert_eq!(
                Activation::Retry
                    .write_chunk(&worker, hold, bytes.clone())
                    .blocking_recv()
                    .expect("published repair answers"),
                Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
            );
        }
        assert_eq!(
            restored
                .queue
                .operation(&id)
                .expect("same original take")
                .id()
                .as_str(),
            "original_take_request"
        );
        assert_eq!(
            worker.finish().blocking_recv().expect("drain answers"),
            Ok(CommitOutcome::Durable)
        );
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
                workspace.open_persistence(profile.path(), window, cx);
            })
        })
        .expect("headless local storage owner");
        cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
            .await;
        (window, workspace)
    }

    async fn commit(workspace: &Entity<Workspace>, cx: &mut TestAppContext) {
        let (stamp, saved) = workspace.update(cx, |workspace, cx| {
            workspace
                .persistence_barrier(cx)
                .expect("full snapshot barrier")
        });
        let result = saved.await.expect("save answers");
        assert!(
            workspace.update(cx, |workspace, cx| workspace
                .record_persistence(stamp, &result, cx)),
            "atomic retirement snapshot is durable"
        );
    }

    async fn finish_local(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) {
        let finished = workspace.update(cx, |workspace, _| {
            workspace.attachment_worker().expect("real worker").finish()
        });
        assert_eq!(
            finished.await.expect("worker drain answers"),
            Ok(CommitOutcome::Durable)
        );
        // The drained worker released its lock. Removing the window would also
        // stop this test app's foreground work, which a later reopen needs.
        let _ = window;
    }

    #[gpui_kit::test]
    async fn explicit_take_retry_uses_resumable_persistence_for_destination_and_frozen_frame(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("private journal");
        let (mut snapshot, id, original, _) = pending_download(&mut journal);
        let companion = serde_json::to_vec(&CompanionDocument::new(
            native(),
            vec![original.retained.clone()],
        ))
        .expect("captured destination");
        let specifications = [
            (
                "destination-interrupted",
                QueueObjectKind::NativeCompanion,
                companion,
                102,
            ),
            (
                "send",
                QueueObjectKind::FrozenSend,
                send_frame(&original, "msg_original_prompt", Some(Delivery::Steer)),
                103,
            ),
        ];
        let mut planned = Vec::new();
        for (role, kind, bytes, label) in specifications {
            let object = QueueObjectId::new(&format!("{}-{role}", id.as_str()))
                .expect("original destination/frame label");
            let blob = BlobId::parse(&format!("{label:032x}")).expect("local byte label");
            let capture = QueueCaptureBytes::new(kind, bytes.clone(), &[blob])
                .expect("complete original capture");
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
                        .expect("unpublished destination/frame"),
                        |_| Ok(())
                    )
                    .is_ok()
            );
            planned.push((role.to_owned(), kind, bytes, object, blob));
        }
        save(&mut journal, &snapshot);
        drop(journal);
        let inodes: Vec<_> = planned
            .iter()
            .map(|(_, _, bytes, _, blob)| partial(&profile, blob, bytes))
            .collect();
        let (window, workspace) = open_local(&profile, cx).await;
        for ((role, kind, bytes, object, blob), inode) in planned.into_iter().zip(inodes) {
            let operation = id.clone();
            let initial_role = role.clone();
            let initial_bytes = bytes.clone();
            let initial = cx
                .update_window(window, |_, window, cx| {
                    workspace.update(cx, |_, cx| {
                        cx.spawn_in(window, async move |this, cx| {
                            Activation::Initial
                                .persist(&this, cx, &operation, &initial_role, kind, initial_bytes)
                                .await
                        })
                    })
                })
                .expect("initial persistence activation");
            assert!(
                initial.await.is_err(),
                "fresh take persistence cannot repair a short file"
            );
            let operation = id.clone();
            let expected = bytes.clone();
            let retry = cx
                .update_window(window, |_, window, cx| {
                    workspace.update(cx, |_, cx| {
                        cx.spawn_in(window, async move |this, cx| {
                            Activation::Retry
                                .persist(&this, cx, &operation, &role, kind, bytes)
                                .await
                        })
                    })
                })
                .expect("explicit retry persistence activation");
            assert_eq!(retry.await.expect("resumed destination/frame"), object);
            assert_eq!(
                fs::metadata(blob_path(&profile, &blob))
                    .expect("same retained label")
                    .ino(),
                inode
            );
            assert_eq!(
                fs::read(blob_path(&profile, &blob)).expect("exact original captured bytes"),
                expected
            );
            workspace.read_with(cx, |workspace, _| {
                assert!(
                    workspace
                        .queue_storage
                        .object(&object)
                        .expect("same owned object")
                        .verified()
                )
            });
        }
        workspace.read_with(cx, |workspace, _| {
            let QueueAction::Take {
                phase: QueueTakePhase::Transferring(transfer),
                ..
            } = workspace
                .queue_storage
                .operation(&id)
                .expect("original identity")
                .action()
            else {
                panic!("local retries grant no acknowledgement");
            };
            assert_eq!(transfer.items[0].index, original.retained.index);
        });
        finish_local(window, &workspace, cx).await;
    }

    fn local_file(item: &MaterializedQueueItem) -> AttachmentId {
        AttachmentId::from(
            item.retained.files[0]
                .descriptor()
                .expect("original local binding")
                .id()
                .as_str(),
        )
    }

    #[gpui_kit::test]
    async fn send_retirement_rolls_back_refusal_and_commits_both_namespaces_before_the_crash_boundary(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("private journal");
        let (snapshot, id, item) = admitted_send(&mut journal);
        let file = local_file(&item);
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        let plan = workspace.update(cx, |workspace, _| {
            let before = serde_json::to_vec(&workspace.queue_storage).expect("original operation");
            let catalog = workspace.attachments.clone();
            let foreign = AttachmentId::from("0000000000000000000000000000ffff");
            assert!(
                crate::queue_enqueue::prepare_retirement(workspace, &id, &[foreign]).is_err(),
                "a caller cannot retire an unrelated attachment identity"
            );
            assert_eq!(
                serde_json::to_vec(&workspace.queue_storage).expect("foreign request refused"),
                before
            );
            workspace.retired_attachment_blobs = (10_000
                ..10_000 + crate::mutation_journal::MAX_ENTRIES)
                .map(|label| {
                    BlobId::parse(&format!("{label:032x}")).expect("unrelated retirement label")
                })
                .collect();
            let retired = workspace.retired_attachment_blobs.clone();
            assert!(
                crate::queue_enqueue::prepare_retirement(
                    workspace,
                    &id,
                    std::slice::from_ref(&file)
                )
                .is_err()
            );
            assert_eq!(
                serde_json::to_vec(&workspace.queue_storage).expect("retained operation"),
                before
            );
            assert_eq!(workspace.attachments, catalog);
            assert_eq!(
                workspace.retired_attachment_blobs, retired,
                "capacity refusal cannot remove the operation first"
            );
            workspace.retired_attachment_blobs.clear();
            let mut invalid = workspace
                .drafts
                .take(&owner())
                .expect("independent native draft");
            let metadata = invalid.content.metadata_json.clone();
            invalid.content.metadata_json = Some("[]".into());
            workspace.drafts.save(owner(), invalid);
            assert!(
                crate::queue_enqueue::prepare_retirement(
                    workspace,
                    &id,
                    std::slice::from_ref(&file)
                )
                .is_err()
            );
            assert_eq!(
                serde_json::to_vec(&workspace.queue_storage).expect("preflight retained operation"),
                before
            );
            assert_eq!(
                workspace.attachments, catalog,
                "full snapshot refusal restores catalog ownership"
            );
            assert!(workspace.retired_attachment_blobs.is_empty());
            let mut valid = workspace
                .drafts
                .take(&owner())
                .expect("unchanged independent draft");
            valid.content.metadata_json = metadata;
            workspace.drafts.save(owner(), valid);
            crate::queue_enqueue::prepare_retirement(workspace, &id, &[])
                .expect("the existing retire API also retains every admitted-send attachment")
        });
        assert_eq!(
            plan.attachments,
            [BlobId::parse(file.as_str()).expect("original blob")]
        );
        assert!(!plan.chunks.is_empty());
        commit(&workspace, cx).await;
        finish_local(window, &workspace, cx).await;
        // A restart here is the former crash window: the operation is gone but
        // both exact retirement namespaces still own their unreleased bytes.
        let (window, workspace) = open_local(&profile, cx).await;
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.queue_storage.operation(&id).is_none());
            assert!(!workspace.attachments.contains_key(&file));
            assert!(
                workspace
                    .retired_attachment_blobs
                    .contains(&plan.attachments[0])
            );
            assert_eq!(
                workspace
                    .queue_storage
                    .retired_blobs()
                    .expect("retained queue intents"),
                plan.chunks
            );
        });
        for blob in plan.chunks.iter().chain(&plan.attachments) {
            assert!(
                blob_path(&profile, blob).is_file(),
                "hydration performs no retirement I/O"
            );
        }
        assert_eq!(
            snapshot
                .queue
                .operation(&id)
                .expect("original request")
                .id()
                .as_str(),
            "original_take_request"
        );
        finish_local(window, &workspace, cx).await;
    }

    #[gpui_kit::test]
    async fn admitted_send_retirement_preserves_other_draft_and_native_queue_attachment_owners(
        cx: &mut TestAppContext,
    ) {
        for native_queue in [false, true] {
            let profile = Profile::new();
            let (mut journal, _) =
                AttachmentJournal::open(&profile.path()).expect("private journal");
            let (_, id, item) = admitted_send(&mut journal);
            let file = local_file(&item);
            drop(journal);
            let (window, workspace) = open_local(&profile, cx).await;
            workspace.update(cx, |workspace, _| {
                let prepared = DraftFile {
                    id: file.clone(),
                    name: Some("retained.txt".into()),
                    media_type: Some("text/plain".into()),
                    byte_len: Some(8),
                    source_document_id: None,
                    content: DraftFileContent::Prepared,
                };
                if native_queue {
                    let other_owner = DraftKey::new(
                        RuntimeId::from("original-host"),
                        "/project",
                        DraftTarget::Session(SessionId::from("ses_other")),
                    )
                    .expect("other native owner");
                    let mut draft = Draft::new("independent retained source");
                    draft.content.files.push(prepared);
                    let queue_owner = QueueOwner::new(
                        other_owner.runtime(),
                        "/project",
                        &SessionId::from("ses_other"),
                        &other_owner,
                    )
                    .expect("other queue owner");
                    let operation = QueueOperation::enqueue(
                        QueueOperationId::new("independent_enqueue_owner").expect("other identity"),
                        queue_owner,
                        QueueConfiguration::capture(item.document.item().send_config()),
                        QueueNativeSource {
                            owner: other_owner,
                            draft,
                        },
                    )
                    .expect("other complete source");
                    workspace
                        .change_queue_storage(|queue, workspace| {
                            queue
                                .admit_operation(operation, |candidate| {
                                    workspace.validate_queue_candidate(candidate)
                                })
                                .map_err(|(error, _)| error)
                        })
                        .expect("other native queue owner admitted");
                } else {
                    let mut draft = workspace
                        .drafts
                        .take(&owner())
                        .expect("newer independent draft");
                    draft.content.files.push(prepared);
                    workspace.drafts.save(owner(), draft);
                }
                let plan = crate::queue_enqueue::prepare_retirement(
                    workspace,
                    &id,
                    std::slice::from_ref(&file),
                )
                .expect("retire only the admitted send");
                assert!(
                    plan.attachments.is_empty(),
                    "another complete owner keeps the catalog descriptor"
                );
                assert!(workspace.attachments.contains_key(&file));
                assert!(workspace.retired_attachment_blobs.is_empty());
                assert!(workspace.queue_storage.operation(&id).is_none());
            });
            commit(&workspace, cx).await;
            assert_eq!(
                fs::read(blob_path(
                    &profile,
                    &BlobId::parse(file.as_str()).expect("owned file")
                ))
                .expect("other owner's bytes remain"),
                b"original"
            );
            finish_local(window, &workspace, cx).await;
        }
    }

    #[derive(Clone, Copy)]
    enum FailedRelease {
        QueueChunk,
        Attachment,
    }

    async fn independent_retirement_failure(cx: &mut TestAppContext, failure: FailedRelease) {
        use std::os::unix::fs::symlink;

        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("private journal");
        let (_, id, item) = admitted_send(&mut journal);
        let file = local_file(&item);
        drop(journal);
        let (window, workspace) = open_local(&profile, cx).await;
        let plan = workspace.update(cx, |workspace, _| {
            crate::queue_enqueue::prepare_retirement(workspace, &id, std::slice::from_ref(&file))
                .expect("atomic retirement plan")
        });
        commit(&workspace, cx).await;
        let failed = match failure {
            FailedRelease::QueueChunk => plan.chunks[0],
            FailedRelease::Attachment => plan.attachments[0],
        };
        let path = blob_path(&profile, &failed);
        let original = fs::read(&path).expect("test-owned original bytes");
        let protected = profile.0.join("protected-independent-test-artifact.bin");
        fs::write(&protected, b"protected independent bytes").expect("protected test artifact");
        fs::remove_file(&path).expect("replace only test-owned retirement fixture");
        symlink(&protected, &path).expect("unsafe failed release fixture");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.retry_enqueue_retirement(window, cx)
            })
        })
        .expect("explicit retry covers both namespaces");
        cx.condition(&workspace, |workspace, _| {
            !workspace.queue.enqueues.any_busy()
        })
        .await;
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.queue_storage.operation(&id).is_none());
            match failure {
                FailedRelease::QueueChunk => {
                    assert_eq!(
                        workspace
                            .queue_storage
                            .retired_blobs()
                            .expect("failed queue intent"),
                        [failed]
                    );
                    assert!(
                        workspace.retired_attachment_blobs.is_empty(),
                        "a queue failure cannot block attachment retirement"
                    );
                }
                FailedRelease::Attachment => {
                    assert!(
                        workspace
                            .queue_storage
                            .retired_blobs()
                            .expect("queue successes")
                            .is_empty()
                    );
                    assert_eq!(
                        workspace.retired_attachment_blobs,
                        [failed],
                        "attachment failure retains only its exact intent"
                    );
                }
            }
        });
        for blob in plan
            .chunks
            .iter()
            .chain(&plan.attachments)
            .filter(|blob| **blob != failed)
        {
            assert!(
                !blob_path(&profile, blob).exists(),
                "independent success releases and is saved"
            );
        }
        assert_eq!(
            fs::read(&protected).expect("unsafe target untouched"),
            b"protected independent bytes"
        );
        finish_local(window, &workspace, cx).await;
        let (window, workspace) = open_local(&profile, cx).await;
        fs::remove_file(&path).expect("repair only test-owned unsafe fixture");
        let mut repaired = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("corrected failed fixture");
        repaired
            .write_all(&original)
            .expect("original bytes at failed label");
        drop(repaired);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.retry_enqueue_retirement(window, cx)
            })
        })
        .expect("explicit failed-intent retry");
        cx.condition(&workspace, |workspace, _| {
            !workspace.queue.enqueues.any_busy()
        })
        .await;
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace
                    .queue_storage
                    .retired_blobs()
                    .expect("queue intents")
                    .is_empty()
            );
            assert!(workspace.retired_attachment_blobs.is_empty());
        });
        assert!(!path.exists());
        finish_local(window, &workspace, cx).await;
    }

    #[gpui_kit::test]
    async fn a_failed_queue_chunk_does_not_lose_or_block_admitted_send_attachment_retirement(
        cx: &mut TestAppContext,
    ) {
        independent_retirement_failure(cx, FailedRelease::QueueChunk).await;
    }

    #[gpui_kit::test]
    async fn a_failed_attachment_does_not_lose_queue_chunk_successes_or_its_own_retry_intent(
        cx: &mut TestAppContext,
    ) {
        independent_retirement_failure(cx, FailedRelease::Attachment).await;
    }

    #[test]
    fn ack_lost_response_restart_keeps_one_companion_original_ids_and_new_typing() {
        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("private journal");
        let (mut snapshot, id, item) = setup(&mut journal);
        let proof = item
            .document
            .files_proof(sha256)
            .expect("original materialization proof");
        assert!(
            journal
                .authorize_queue_acknowledgement(&id, std::slice::from_ref(&proof))
                .is_err(),
            "uncommitted destination cannot acknowledge"
        );
        save(&mut journal, &snapshot);
        let permit = journal
            .authorize_queue_acknowledgement(&id, std::slice::from_ref(&proof))
            .expect("worker-owned permit");
        snapshot
            .queue
            .mark_unknown(&id)
            .expect("save acknowledgement uncertainty before POST");
        snapshot.drafts[0].1.edit_text("new typing".into(), 10..10);
        save(&mut journal, &snapshot);
        drop(journal);
        let (mut journal, loaded) = AttachmentJournal::open(&profile.path()).expect("restart");
        let LoadedJournal::Present(mut restored) = loaded else {
            panic!("complete saved state");
        };
        assert_eq!(restored.drafts[0].1.text, "new typing");
        assert_eq!(restored.queue.draft_companions(&owner()).len(), 1);
        assert!(
            matches!(
                restored
                    .queue
                    .operation(&id)
                    .expect("original operation")
                    .action(),
                QueueAction::Take {
                    phase: QueueTakePhase::AcknowledgementUnknown(_),
                    ..
                }
            ),
            "hydration preserves uncertainty without replay"
        );
        let observed = json(
            json!({"receipt":{"version":1,"kind":"take","requestId":"original_take_request",
            "sessionId":"ses_queue","directory":"/project","requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["q_original"]},
            "state":"acknowledged","revision":2}),
        );
        // Explicit Check status may commit this observed result with a fresh
        // worker permit. It neither appends again nor resubmits a take.
        let fresh = journal
            .authorize_queue_acknowledgement(&id, &[proof])
            .expect("same original byte authority after restart");
        assert_eq!(permit, fresh);
        restored
            .queue
            .record_take_acknowledged(&fresh, observed, |_| Ok(()))
            .expect("lost response reconciled");
        save(&mut journal, &restored);
        assert_eq!(restored.queue.draft_companions(&owner()).len(), 1);
        let QueueAction::Take { request, .. } = restored
            .queue
            .operation(&id)
            .expect("original identity")
            .action()
        else {
            panic!("take");
        };
        assert_eq!(
            request.value().expect("original request")["itemIds"],
            json!(["q_original"])
        );
        assert_eq!(
            restored
                .queue
                .operation(&id)
                .expect("original ID")
                .id()
                .as_str(),
            "original_take_request"
        );
    }

    #[test]
    fn companion_consumption_is_exact_and_refused_publication_keeps_its_owner() {
        let profile = Profile::new();
        let (mut journal, _) = AttachmentJournal::open(&profile.path()).expect("journal");
        let (mut snapshot, id, _) = setup(&mut journal);
        let captured = snapshot.queue.draft_companions(&owner());
        let error = snapshot
            .queue
            .consume_draft_companions(&owner(), &captured, |_| {
                Err(crate::mutation_journal::JournalError::Invalid(
                    "full snapshot refused",
                ))
            });
        assert!(error.is_err());
        assert_eq!(snapshot.queue.draft_companions(&owner()).len(), 1);
        assert!(
            snapshot.queue.retire_operation(&id, |_| Ok(())).is_err(),
            "a live composer root cannot retire"
        );
        let other = DraftKey::new(
            RuntimeId::from("replacement-host"),
            "/project",
            DraftTarget::Session(SessionId::from("ses_queue")),
        )
        .expect("other runtime");
        assert!(
            snapshot.queue.draft_companions(&other).is_empty(),
            "host switches never move a companion to a colliding session"
        );
        assert!(
            snapshot
                .queue
                .consume_draft_companions(&other, &captured, |_| Ok(()))
                .is_err()
        );
        assert_eq!(snapshot.drafts[0].0.runtime().as_str(), "original-host");
        JournalView::from(&snapshot)
            .validate()
            .expect("refusal leaves the previous snapshot intact");
    }
}
