//! Queue ownership contracts. File operations run only inside test-owned profiles.

use super::*;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use openchamber_native_core::draft::Draft;
use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
use openchamber_native_transport::queue_manifest::{
    QueuePayloadDocument, QueuePayloadFilesProof, QueuePayloadUse,
};

use crate::attachment_catalog::{PreparationHold, PreparedAttachment};
use crate::attachment_io::{AttachmentFailure, AttachmentJournal};
use crate::attachment_spool::SpoolError;
use crate::journal_worker::{CommitOutcome, JournalWorker};
use crate::mutation_journal::{JournalCommit, JournalSnapshot, JournalView, LoadedJournal};
use crate::queue_capture::{QUEUE_CAPTURE_BYTES, QUEUE_SEND_BYTES, QueueCaptureBytes};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;

        let path = PathBuf::from(format!(
            "/tmp/opencode/queue-ownership-{}-{}",
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

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn blob(value: u128) -> BlobId {
    BlobId::parse(&format!("{value:032x}")).expect("fixed test blob label")
}

fn source() -> QueueNativeSource {
    let owner = DraftKey::new(
        RuntimeId::from("runtime"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_queue")),
    )
    .expect("queue draft owner");
    let mut draft = Draft::new("retained λ 👩");
    draft.content.files.push(DraftFile {
        id: AttachmentId::from("reference-file"),
        name: Some("notes.txt".into()),
        media_type: None,
        byte_len: None,
        source_document_id: Some(AttachmentId::from("source-group")),
        content: DraftFileContent::Reference {
            uri: "data:text/plain;base64,aGVsbG8=".into(),
        },
    });
    QueueNativeSource { owner, draft }
}

fn operation(source: QueueNativeSource) -> QueueOperation {
    let owner = QueueOwner::new(
        source.owner.runtime(),
        source.owner.directory(),
        &SessionId::from("ses_queue"),
        &source.owner,
    )
    .expect("exact queue owner");
    QueueOperation::enqueue(
        QueueOperationId::new("enqueue_original").expect("request identity"),
        owner,
        QueueConfiguration::capture(&QueuedSendConfig {
            provider_id: "provider".into(),
            model_id: "model".into(),
            agent: Some("build".into()),
            variant: None,
        }),
        source,
    )
    .expect("complete enqueue source")
}

fn admit(snapshot: &mut JournalSnapshot, operation: QueueOperation) {
    let mut queue = std::mem::take(&mut snapshot.queue);
    let admitted = queue.admit_operation(operation, |candidate| {
        let mut view = JournalView::from(&*snapshot);
        view.queue = candidate;
        view.validate()
    });
    snapshot.queue = queue;
    admitted
        .map_err(|(error, _)| error)
        .expect("complete source passes canonical snapshot admission");
}

fn plan(
    snapshot: &mut JournalSnapshot,
    bytes: &QueueCaptureBytes,
    kind: QueueObjectKind,
) -> QueueObjectId {
    let id = QueueObjectId::new("native-source").expect("local object label");
    let object = QueueObject::planned(
        id.clone(),
        snapshot.queue.operations()[0].id().clone(),
        kind,
        1,
        bytes.reference().clone(),
    )
    .expect("held complete capture");
    let mut queue = std::mem::take(&mut snapshot.queue);
    let admitted = queue.admit_object(object, |candidate| {
        let mut view = JournalView::from(&*snapshot);
        view.queue = candidate;
        view.validate()
    });
    snapshot.queue = queue;
    assert!(admitted.is_ok(), "held chunks pass canonical admission");
    id
}

fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
    assert!(
        matches!(journal.save(snapshot), Ok(JournalCommit::Durable)),
        "snapshot is durable"
    );
}

#[test]
fn rejected_full_snapshot_admission_returns_complete_source_and_preserves_unrelated_drafts() {
    let mut snapshot = JournalSnapshot::default();
    for id in ["ses_first", "ses_second"] {
        let owner = DraftKey::new(
            RuntimeId::from("other"),
            "/project",
            DraftTarget::Session(SessionId::from(id)),
        )
        .expect("unrelated owner");
        snapshot.drafts.push((
            owner,
            Draft::new("\0".repeat(crate::mutation_journal::MAX_TEXT)),
        ));
    }
    JournalView::from(&snapshot)
        .validate()
        .expect("unrelated escaped drafts fit the journal");
    let mut retained = source();
    retained.draft.text = "\0".repeat(crate::mutation_journal::MAX_TEXT);
    retained.draft.selection = 0..0;
    let expected = retained.clone();
    let mut queue = std::mem::take(&mut snapshot.queue);
    let rejected = queue.admit_operation(operation(retained), |candidate| {
        let mut view = JournalView::from(&snapshot);
        view.queue = candidate;
        view.validate()
    });
    let Err((_, returned)) = rejected else {
        panic!("encoded aggregate crosses 16 MiB");
    };
    let Some(QueueSource::Inline(returned)) = returned.source() else {
        panic!("source remains expanded");
    };
    assert_eq!(returned.owner, expected.owner);
    assert_eq!(
        returned.draft, expected.draft,
        "all rejected source content survives"
    );
    assert!(
        queue.operations().is_empty(),
        "refused operation acquires no ownership"
    );
    assert_eq!(snapshot.drafts.len(), 2, "unrelated drafts remain intact");
}

#[test]
fn source_publication_requires_durable_chunks_and_worker_hashing_and_survives_restart() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("test journal");
    let original = source();
    let capture =
        QueueCaptureBytes::native_source(&original, &[blob(1)]).expect("small native companion");
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation(original.clone()));
    let id = plan(&mut snapshot, &capture, QueueObjectKind::NativeCompanion);
    let operation = snapshot.queue.operations()[0].id().clone();
    let hold = snapshot
        .queue
        .object(&id)
        .expect("held object")
        .hold(0)
        .expect("chunk hold");
    assert_eq!(
        journal.put_queue(&hold, capture.bytes()),
        Err(AttachmentFailure::Unauthorized)
    );
    assert!(
        !scratch.blob(&blob(1)).exists(),
        "live metadata creates no file authority"
    );
    save(&mut journal, &snapshot);
    assert_eq!(
        journal.put_queue(&hold, b"wrong"),
        Err(AttachmentFailure::Spool(SpoolError::ContentMismatch))
    );
    assert!(
        !scratch.blob(&blob(1)).exists(),
        "wrong captured bytes never occupy the held identity"
    );
    let receipt = journal
        .put_queue(&hold, capture.bytes())
        .expect("durable held bytes");
    snapshot
        .queue
        .record_chunk(&hold, &receipt)
        .expect("exact durable chunk receipt");
    let verification = snapshot.queue.object(&id).expect("object").verification();
    assert_eq!(
        journal.verify_queue(&verification),
        Err(AttachmentFailure::Unauthorized),
        "an unsaved chunk receipt cannot authorize verification"
    );
    save(&mut journal, &snapshot);
    let mut stale = verification.clone();
    stale.generation += 1;
    assert_eq!(
        journal.verify_queue(&stale),
        Err(AttachmentFailure::Unauthorized)
    );
    let proof = journal
        .verify_queue(&verification)
        .expect("worker hashes committed byte stream");
    snapshot
        .queue
        .publish_source(&operation, &proof)
        .expect("publish exact native source");
    assert!(
        snapshot
            .queue
            .references_native(&AttachmentId::from("source-group")),
        "spooled source groups remain roots"
    );
    assert!(
        snapshot
            .queue
            .retire_operation(&operation, |_| Ok(()))
            .is_err(),
        "unresolved source never retires"
    );
    save(&mut journal, &snapshot);
    drop(journal);
    let (journal, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("restart under same profile lock");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("saved v8 snapshot");
    };
    let Some(QueueSource::Companion(restored_id)) = restored.queue.operations()[0].source() else {
        panic!("compact companion survives");
    };
    assert_eq!(restored_id, &id);
    let bytes = journal
        .read_queue(&hold.chunk.descriptor().expect("descriptor"))
        .expect("restored byte ownership");
    let restored_source: QueueNativeSource =
        serde_json::from_slice(bytes.as_slice()).expect("lossless native source");
    assert_eq!(restored_source.owner, original.owner);
    assert_eq!(restored_source.draft, original.draft);
}

#[test]
fn failed_save_revokes_queue_authority_and_same_length_corruption_never_verifies() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("test journal");
    let capture = QueueCaptureBytes::new(QueueObjectKind::Payload, b"saved".to_vec(), &[blob(2)])
        .expect("opaque capture");
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation(source()));
    let id = plan(&mut snapshot, &capture, QueueObjectKind::Payload);
    let hold = snapshot
        .queue
        .object(&id)
        .expect("object")
        .hold(0)
        .expect("hold");
    save(&mut journal, &snapshot);
    let receipt = journal
        .put_queue(&hold, capture.bytes())
        .expect("durable bytes");
    snapshot
        .queue
        .record_chunk(&hold, &receipt)
        .expect("chunk receipt");
    save(&mut journal, &snapshot);
    let verification = snapshot.queue.object(&id).expect("object").verification();
    let mut invalid = JournalSnapshot::default();
    invalid.drafts.push((
        source().owner,
        Draft::new("x".repeat(crate::mutation_journal::MAX_TEXT + 1)),
    ));
    assert!(
        journal.save(&invalid).is_err(),
        "invalid snapshot leaves the last file intact"
    );
    assert_eq!(
        journal.verify_queue(&verification),
        Err(AttachmentFailure::NoDurableAuthority)
    );
    save(&mut journal, &snapshot);
    fs::write(scratch.blob(&blob(2)), b"other").expect("inject same-length corruption");
    assert_eq!(
        journal.verify_queue(&verification),
        Err(AttachmentFailure::Spool(SpoolError::ContentMismatch))
    );
}

#[test]
fn queue_chunk_and_frozen_send_limits_are_separate_without_allocating_fixture_payloads() {
    let descriptors = |sizes: &[u64]| -> Vec<BlobDescriptor> {
        sizes
            .iter()
            .enumerate()
            .map(|(index, size)| BlobDescriptor::new(blob(index as u128 + 10), *size, [0; 32]))
            .collect()
    };
    let mib = 1024 * 1024;
    let sizes = [20 * mib, 20 * mib, 10 * mib];
    assert!(
        QueueSpoolRef::new(
            QUEUE_CAPTURE_BYTES,
            [0; 32],
            descriptors(&sizes),
            QueueObjectKind::Payload
        )
        .is_ok()
    );
    assert!(
        QueueSpoolRef::new(
            QUEUE_CAPTURE_BYTES + 1,
            [0; 32],
            descriptors(&[20 * mib, 20 * mib, 10 * mib + 1]),
            QueueObjectKind::Payload
        )
        .is_err()
    );
    let sizes = [
        20 * mib,
        20 * mib,
        20 * mib,
        20 * mib,
        20 * mib,
        20 * mib,
        8 * mib,
    ];
    assert!(
        QueueSpoolRef::new(
            QUEUE_SEND_BYTES,
            [0; 32],
            descriptors(&sizes),
            QueueObjectKind::FrozenSend
        )
        .is_ok()
    );
    assert!(
        QueueSpoolRef::new(
            QUEUE_SEND_BYTES,
            [0; 32],
            descriptors(&sizes),
            QueueObjectKind::NativeCompanion
        )
        .is_err()
    );
    assert!(
        QueueSpoolRef::new(
            20 * mib + 1,
            [0; 32],
            descriptors(&[20 * mib + 1]),
            QueueObjectKind::FrozenSend
        )
        .is_err()
    );
}

#[test]
fn serialized_worker_owns_queue_put_verification_read_and_finish_under_one_lock() {
    let scratch = Scratch::new();
    let (worker, hydration) = JournalWorker::start(scratch.journal());
    assert!(matches!(
        hydration.blocking_recv(),
        Ok(Ok(LoadedJournal::Missing))
    ));
    let mut snapshot = JournalSnapshot::default();
    let capture =
        QueueCaptureBytes::native_source(&source(), &[blob(3)]).expect("native companion");
    admit(&mut snapshot, operation(source()));
    let id = plan(&mut snapshot, &capture, QueueObjectKind::NativeCompanion);
    let hold = snapshot
        .queue
        .object(&id)
        .expect("object")
        .hold(0)
        .expect("hold");
    let copy = |snapshot: &JournalSnapshot| JournalSnapshot {
        queue: snapshot.queue.clone(),
        ..JournalSnapshot::default()
    };
    assert_eq!(
        worker
            .submit(copy(&snapshot))
            .blocking_recv()
            .expect("hold barrier"),
        Ok(CommitOutcome::Durable)
    );
    let receipt = worker
        .put_queue_chunk(hold.clone(), capture.bytes().to_vec())
        .blocking_recv()
        .expect("put answered")
        .expect("durable queue put");
    snapshot
        .queue
        .record_chunk(&hold, &receipt)
        .expect("record chunk");
    assert_eq!(
        worker
            .submit(copy(&snapshot))
            .blocking_recv()
            .expect("chunk barrier"),
        Ok(CommitOutcome::Durable)
    );
    let verification = snapshot.queue.object(&id).expect("object").verification();
    let proof = worker
        .verify_queue_capture(verification)
        .blocking_recv()
        .expect("verification answered")
        .expect("whole stream matches");
    snapshot
        .queue
        .publish_source(
            &QueueOperationId::new("enqueue_original").expect("operation"),
            &proof,
        )
        .expect("publish native companion");
    assert_eq!(
        worker
            .submit(copy(&snapshot))
            .blocking_recv()
            .expect("publication barrier"),
        Ok(CommitOutcome::Durable)
    );
    let read = worker.read_queue_chunk(receipt.descriptor);
    assert_eq!(
        worker
            .finish()
            .blocking_recv()
            .expect("finish drains accepted read"),
        Ok(CommitOutcome::Durable)
    );
    assert_eq!(
        read.blocking_recv()
            .expect("read answered")
            .expect("verified bytes")
            .as_slice(),
        capture.bytes()
    );
    let (journal, _) = AttachmentJournal::open(&scratch.journal())
        .expect("finish released the existing profile lock");
    drop(journal);
}

fn queue_json(value: serde_json::Value) -> QueueJson {
    QueueJson::new(serde_json::to_vec(&value).expect("test JSON")).expect("bounded metadata")
}

/// Encode a small synthetic server index in the backend's documented schema order.
/// Parsing and materialization still use the production transport with real SHA-256.
fn attachment_payload(manifest: bool) -> (QueuePayloadDocument, QueueIndexedPayload) {
    attachment_payload_with_contexts(manifest, &[])
}

fn attachment_payload_with_contexts(
    manifest: bool,
    context_ids: &[&str],
) -> (QueuePayloadDocument, QueueIndexedPayload) {
    let config = serde_json::json!({"providerID":"provider","modelID":"model","agent":"build"});
    let mut item = if manifest {
        serde_json::json!({"format":"manifest-v1","content":"display","text":"deliver", "sendConfig":config,
            "attachmentManifest":[{"blobId":"ab".repeat(32),"requestId":"file_original","attachmentId":"file1",
                "filename":"notes.txt","mimeType":"text/plain","size":8,"sha256":hex_digest(&sha256(b"original"))}]})
    } else {
        serde_json::json!({"content":"display","text":"deliver","sendConfig":config,
            "attachments":[{"id":"file1","filename":"notes.txt","mimeType":"text/plain","dataUrl":"data:,original"}]})
    };
    if !context_ids.is_empty() {
        assert!(
            manifest,
            "explicit context fixture uses the original manifest protocol"
        );
        item["promptParts"] = serde_json::json!({"context": context_ids.iter().map(|id|
            serde_json::json!({"id":id,"text":format!("Original context for {id}")})).collect::<Vec<_>>()});
    }
    let capture = serde_json::to_vec(&serde_json::json!({"directory":"/project","item":item}))
        .expect("capture JSON");
    let document =
        QueuePayloadDocument::from_bytes(capture, "/project", QueuePayloadUse::Enqueue, sha256)
            .expect("complete typed attachment capture");
    let len = document.bytes().len();
    let digest = hex_digest(&document.sha256());
    let pointer = format!(
        r#"{{"version":1,"encoding":"utf-8-json","totalBytes":{len},"sha256":"{digest}","chunks":[{{"attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":{len},"sha256":"{digest}","blobId":"{}","requestId":"upload_original","ordinal":0}}]}}"#,
        "cd".repeat(32)
    );
    let roots = if manifest {
        format!(r#"["{}"]"#, "ab".repeat(32))
    } else {
        "[]".to_owned()
    };
    let legacy = if manifest {
        ""
    } else {
        r#","legacyAttachmentIds":["file1"]"#
    };
    let entries = 1 + context_ids.len();
    let contexts = serde_json::to_string(context_ids).expect("exact server fixture context IDs");
    let index = format!(
        r#"{{"directory":"/project","sendConfig":{{"providerID":"provider","modelID":"model","agent":"build"}},"preview":"display","entryCount":{entries},"contextIds":{contexts},"attachmentBlobIds":{roots},"promptBytes":20,"admissionBytes":32,"structuredBytes":0{legacy}}}"#
    );
    let binding = format!(r#"{{"payloadManifest":{pointer},"payloadIndex":{index}}}"#);
    let seal = hex_digest(&sha256(binding.as_bytes()));
    let body = format!(
        r#"{{"format":"payload-v1","payloadManifest":{pointer},"payloadIndex":{index},"payloadIndexHash":"{seal}"}}"#
    );
    let indexed = QueueIndexedPayload::from_body(body.as_bytes(), "/project", sha256)
        .expect("server index uses the real schema-ordered SHA-256");
    (document, indexed)
}

fn retain_verified_capture(
    journal: &mut AttachmentJournal,
    snapshot: &mut JournalSnapshot,
    name: &str,
    bytes: &QueueCaptureBytes,
    kind: QueueObjectKind,
) -> QueueObjectId {
    let id = QueueObjectId::new(name).expect("local capture label");
    let operation = snapshot.queue.operations()[0].id().clone();
    let object = QueueObject::planned(id.clone(), operation, kind, 1, bytes.reference().clone())
        .expect("held capture");
    let mut queue = std::mem::take(&mut snapshot.queue);
    let result = queue.admit_object(object, |candidate| {
        let mut view = JournalView::from(&*snapshot);
        view.queue = candidate;
        view.validate()
    });
    snapshot.queue = queue;
    assert!(result.is_ok(), "capture fits the complete journal");
    save(journal, snapshot);
    for ordinal in 0..bytes.reference().chunks().len() {
        let hold = snapshot
            .queue
            .object(&id)
            .expect("object")
            .hold(ordinal)
            .expect("exact chunk hold");
        let receipt = journal
            .put_queue(&hold, bytes.chunk(ordinal).expect("captured chunk"))
            .expect("durable bytes");
        snapshot
            .queue
            .record_chunk(&hold, &receipt)
            .expect("retain durable chunk receipt");
    }
    save(journal, snapshot);
    let proof = journal
        .verify_queue(&snapshot.queue.object(&id).expect("object").verification())
        .expect("whole-stream proof");
    snapshot
        .queue
        .record_verified(&proof)
        .expect("record verification");
    save(journal, snapshot);
    id
}

struct TakeFixture {
    snapshot: JournalSnapshot,
    id: QueueOperationId,
    transfer: QueueTransfer,
    destination: QueueDestination,
    acknowledge: QueueJson,
    proof: QueuePayloadFilesProof,
    local_blob: BlobId,
}

fn take_fixture(
    journal: &mut AttachmentJournal,
    manifest: bool,
    local_bytes: &[u8],
) -> TakeFixture {
    take_fixture_with_contexts(journal, manifest, local_bytes, &[])
}

fn take_fixture_with_contexts(
    journal: &mut AttachmentJournal,
    manifest: bool,
    local_bytes: &[u8],
    context_ids: &[&str],
) -> TakeFixture {
    let (document, indexed) = attachment_payload_with_contexts(manifest, context_ids);
    let materialized = indexed
        .materialize(document.bytes().to_vec(), QueuePayloadUse::Enqueue, sha256)
        .expect("original payload and index match");
    let proof = materialized
        .files_proof(sha256)
        .expect("original byte proof");
    let draft_owner = source().owner;
    let owner = QueueOwner::new(
        draft_owner.runtime(),
        "/project",
        &SessionId::from("ses_queue"),
        &draft_owner,
    )
    .expect("captured take owner");
    let id = QueueOperationId::new("take_original").expect("take request identity");
    let request = queue_json(
        serde_json::json!({"directory":"/project","format":"payload-v1","itemIds":["queued1"]}),
    );
    let operation = QueueOperation::take(
        id.clone(),
        owner,
        QueueConfiguration::capture(document.item().send_config()),
        QueueTakePurpose::Edit,
        request,
    );
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation);
    let hold = PreparationHold {
        owner: draft_owner.clone(),
        id: AttachmentId::from(blob(100).as_str()),
        generation: 1,
        blob: blob(100),
    };
    snapshot.preparations.push(hold.clone());
    save(journal, &snapshot);
    let receipt = journal
        .put(&hold, local_bytes)
        .expect("captured local file bytes");
    snapshot.attachments.insert(
        hold.id.clone(),
        PreparedAttachment {
            blob: receipt.descriptor,
            staging: None,
        },
    );
    snapshot.preparations.clear();
    let payload = QueueCaptureBytes::new(
        QueueObjectKind::Payload,
        document.bytes().to_vec(),
        &[blob(101)],
    )
    .expect("original payload spool plan");
    let payload = retain_verified_capture(
        journal,
        &mut snapshot,
        "payload",
        &payload,
        QueueObjectKind::Payload,
    );
    let mut draft = Draft::new("existing composer\n\ndeliver");
    draft.content.files.push(DraftFile {
        id: hold.id.clone(),
        name: Some("notes.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(local_bytes.len() as u64),
        source_document_id: None,
        content: DraftFileContent::Prepared,
    });
    let companion = QueueCaptureBytes::native_source(
        &QueueNativeSource {
            owner: draft_owner,
            draft,
        },
        &[blob(102)],
    )
    .expect("complete native edit destination");
    let companion = retain_verified_capture(
        journal,
        &mut snapshot,
        "destination",
        &companion,
        QueueObjectKind::NativeCompanion,
    );
    let descriptor = receipt.descriptor;
    let local = hold.id.to_string();
    let file = if manifest {
        QueueFileBinding::Manifest {
            blob: [0xab; 32],
            local,
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
        }
    } else {
        QueueFileBinding::Legacy {
            ordinal: 0,
            local,
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
        }
    };
    let transfer = QueueTransfer {
        receipt: queue_json(
            serde_json::json!({"receipt":{"version":1,"kind":"take","requestId":"take_original",
            "sessionId":"ses_queue","directory":"/project","requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["queued1"]},
            "state":"transferring","revision":1}),
        ),
        items: vec![QueueTransferItem {
            item: "queued1".into(),
            index: QueueJson::new(indexed.body().expect("sealed index bytes"))
                .expect("bounded index"),
            payload,
            files: vec![file],
        }],
    };
    TakeFixture {
        snapshot,
        id,
        transfer,
        destination: QueueDestination::Draft { companion },
        acknowledge: queue_json(serde_json::json!({"directory":"/project"})),
        proof,
        local_blob: hold.blob,
    }
}

#[test]
fn take_acknowledgement_binds_manifest_and_legacy_files_to_original_materialized_bytes() {
    for manifest in [false, true] {
        for local_bytes in [b"original".as_slice(), b"tampered".as_slice()] {
            let scratch = Scratch::new();
            let (mut journal, _) =
                AttachmentJournal::open(&scratch.journal()).expect("test journal");
            let mut fixture = take_fixture(&mut journal, manifest, local_bytes);
            let mut queue = std::mem::take(&mut fixture.snapshot.queue);
            queue
                .record_take_transfer(
                    &fixture.id,
                    fixture.transfer.clone(),
                    &fixture.snapshot.attachments,
                    |candidate| {
                        let mut view = JournalView::from(&fixture.snapshot);
                        view.queue = candidate;
                        view.validate()
                    },
                )
                .expect("retain complete transfer metadata");
            queue
                .prepare_take_destination(
                    &fixture.id,
                    fixture.destination,
                    fixture.acknowledge,
                    &fixture.snapshot.attachments,
                    |candidate| {
                        let mut view = JournalView::from(&fixture.snapshot);
                        view.queue = candidate;
                        view.validate()
                    },
                )
                .expect("metadata alone is insufficient for acknowledgement");
            fixture.snapshot.queue = queue;
            assert_eq!(
                journal.authorize_queue_acknowledgement(
                    &fixture.id,
                    std::slice::from_ref(&fixture.proof)
                ),
                Err(AttachmentFailure::Unauthorized),
                "an unsaved destination grants no permit"
            );
            save(&mut journal, &fixture.snapshot);
            drop(journal);
            let (journal, loaded) =
                AttachmentJournal::open(&scratch.journal()).expect("reload committed take");
            assert!(
                matches!(loaded, LoadedJournal::Present(_)),
                "the take survives restart without automatic acknowledgement"
            );
            assert_eq!(
                journal.authorize_queue_acknowledgement(&fixture.id, &[]),
                Err(AttachmentFailure::Unauthorized),
                "omitting materialization proof keeps server ownership"
            );
            let result = journal
                .authorize_queue_acknowledgement(&fixture.id, std::slice::from_ref(&fixture.proof));
            if local_bytes == b"original" {
                assert!(
                    result.is_ok(),
                    "the original file bytes authorize an explicit acknowledgement"
                );
                fs::write(scratch.blob(&fixture.local_blob), b"tampered")
                    .expect("same-length local corruption");
                assert_eq!(
                    journal.authorize_queue_acknowledgement(
                        &fixture.id,
                        std::slice::from_ref(&fixture.proof)
                    ),
                    Err(AttachmentFailure::Spool(SpoolError::ContentMismatch)),
                    "the worker rereads file bytes for every permit"
                );
            } else {
                assert_eq!(
                    result,
                    Err(AttachmentFailure::Spool(SpoolError::ContentMismatch)),
                    "unrelated same-length bytes cannot release the original server file"
                );
            }
        }
    }
}

#[test]
fn refused_take_preflights_keep_previous_phases_and_all_original_metadata() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("test journal");
    let mut fixture = take_fixture(&mut journal, true, b"original");
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("requested queue metadata");
    let error = fixture.snapshot.queue.record_take_transfer(
        &fixture.id,
        fixture.transfer.clone(),
        &fixture.snapshot.attachments,
        |candidate| {
            assert!(
                matches!(
                    candidate.operation(&fixture.id).expect("take").action(),
                    QueueAction::Take {
                        phase: QueueTakePhase::Transferring(_),
                        ..
                    }
                ),
                "preflight sees the proposed transition"
            );
            Err(JournalError::Invalid("full journal capacity refused"))
        },
    );
    assert!(error.is_err(), "full snapshot refusal propagates");
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("retained metadata"),
        before,
        "refusal preserves request identity, configuration, roots and chunk states"
    );
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    queue
        .record_take_transfer(
            &fixture.id,
            fixture.transfer,
            &fixture.snapshot.attachments,
            |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            },
        )
        .expect("canonical full snapshot permits transfer");
    fixture.snapshot.queue = queue;
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("transferring metadata");
    let error = fixture.snapshot.queue.prepare_take_destination(
        &fixture.id,
        fixture.destination,
        fixture.acknowledge,
        &fixture.snapshot.attachments,
        |candidate| {
            assert!(
                candidate.local_take(&fixture.id).is_some(),
                "preflight sees the proposed local destination"
            );
            Err(JournalError::Invalid("full journal capacity refused"))
        },
    );
    assert!(
        error.is_err(),
        "destination publication respects full journal refusal"
    );
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("retained metadata"),
        before,
        "refusal preserves the complete transferring phase"
    );
    assert!(
        fixture.snapshot.queue.local_take(&fixture.id).is_none(),
        "refused destination grants no acknowledgement eligibility"
    );
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("previous snapshot remains valid");
}

#[test]
fn enqueue_retry_keeps_the_exact_authorization_across_restart_and_refused_preflight() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let original = source();
    let companion =
        QueueCaptureBytes::native_source(&original, &[blob(201)]).expect("native companion");
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation(original));
    let id = snapshot.queue.operations()[0].id().clone();
    let object = retain_verified_capture(
        &mut journal,
        &mut snapshot,
        "retry-source",
        &companion,
        QueueObjectKind::NativeCompanion,
    );
    let proof = journal
        .verify_queue(
            &snapshot
                .queue
                .object(&object)
                .expect("source")
                .verification(),
        )
        .expect("whole source proof");
    snapshot
        .queue
        .publish_source(&id, &proof)
        .expect("publish source");
    let (document, indexed) = attachment_payload(true);
    let bytes = QueueCaptureBytes::new(
        QueueObjectKind::Payload,
        document.bytes().to_vec(),
        &[blob(202)],
    )
    .expect("payload bytes");
    let payload = retain_verified_capture(
        &mut journal,
        &mut snapshot,
        "retry-payload",
        &bytes,
        QueueObjectKind::Payload,
    );
    let pointer: serde_json::Value =
        serde_json::from_slice(&indexed.pointer().body().expect("input pointer")).expect("JSON");
    snapshot
        .queue
        .prepare_enqueue(
            &id,
            payload,
            queue_json(serde_json::json!({"directory":"/project","item":pointer})),
            |_| Ok(()),
        )
        .expect("ready enqueue");
    let receipt = queue_json(serde_json::json!({"receipt":{
        "version":1,"kind":"enqueue","requestId":"enqueue_original","sessionId":"ses_queue",
        "directory":"/project","requestHash":"ab".repeat(32),"acceptedAt":1,
        "itemId":"queued_original","messageId":"msg_original","contextIds":[]
    },"state":"uncertain","revision":1,"attempt":0}));
    snapshot
        .queue
        .record_enqueue_accepted(&id, receipt.clone(), |_| Ok(()))
        .expect("accepted immutable receipt");
    let retry = queue_json(serde_json::json!({"directory":"/project","attempt":1}));
    let before = serde_json::to_vec(&snapshot.queue).expect("accepted state");
    assert!(
        snapshot
            .queue
            .prepare_enqueue_retry(&id, retry.clone(), |_| Err(JournalError::Invalid(
                "full journal refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged"),
        before,
        "refusal leaves the accepted owner intact"
    );
    for body in [
        serde_json::json!({"directory":"/project","attempt":0}),
        serde_json::json!({"directory":"/project","attempt":2}),
        serde_json::json!({"directory":"/other","attempt":1}),
    ] {
        assert!(
            snapshot
                .queue
                .prepare_enqueue_retry(&id, queue_json(body), |_| Ok(()))
                .is_err()
        );
    }
    snapshot
        .queue
        .prepare_enqueue_retry(&id, retry.clone(), |_| Ok(()))
        .expect("one explicit retry");
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) = AttachmentJournal::open(&scratch.journal()).expect("restart");
    let LoadedJournal::Present(mut snapshot) = loaded else {
        panic!("retained retry");
    };
    let QueueAction::Enqueue {
        phase:
            QueueEnqueuePhase::RetryPrepared {
                retry: restored,
                receipt: retained,
                ..
            },
        ..
    } = snapshot
        .queue
        .operation(&id)
        .expect("original identity")
        .action()
    else {
        panic!("saved retry authorization");
    };
    assert_eq!(restored, &retry, "retry body and attempt remain exact");
    assert_eq!(
        retained, &receipt,
        "immutable receipt and original admission IDs survive"
    );
    let before = serde_json::to_vec(&snapshot.queue).expect("saved authorization");
    assert!(
        snapshot
            .queue
            .record_enqueue_accepted(&id, receipt.clone(), |_| Ok(()))
            .is_err(),
        "an observation before the authorized attempt cannot discard the retry"
    );
    let mut changed = receipt.value().expect("retained receipt");
    changed["receipt"]["messageId"] = serde_json::json!("msg_replacement");
    changed["attempt"] = serde_json::json!(1);
    assert!(
        snapshot
            .queue
            .record_enqueue_accepted(&id, queue_json(changed), |_| Ok(()))
            .is_err(),
        "lookup cannot replace the accepted message identity"
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged retry"),
        before
    );
    let mut observed = receipt.value().expect("original immutable receipt");
    observed["attempt"] = serde_json::json!(1);
    observed["state"] = serde_json::json!("queued");
    snapshot
        .queue
        .record_enqueue_accepted(&id, queue_json(observed), |_| Ok(()))
        .expect("observation settles exactly the authorized attempt");
    let before = serde_json::to_vec(&snapshot.queue).expect("settled attempt");
    assert!(
        snapshot
            .queue
            .record_enqueue_accepted(&id, receipt, |_| Ok(()))
            .is_err(),
        "a delayed response cannot regress the accepted attempt"
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("retained accepted attempt"),
        before
    );
}

fn native_text() -> QueueNativeSource {
    QueueNativeSource {
        owner: source().owner,
        draft: Draft::new("original native text"),
    }
}

fn captured_owner(native: &QueueNativeSource) -> QueueOwner {
    QueueOwner::new(
        native.owner.runtime(),
        native.owner.directory(),
        &SessionId::from("ses_queue"),
        &native.owner,
    )
    .expect("exact original native owner")
}

fn captured_config() -> QueueConfiguration {
    QueueConfiguration::capture(&QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "model".into(),
        agent: Some("build".into()),
        variant: None,
    })
}

fn prefix_intent(
    native: &QueueNativeSource,
    companions: Vec<DraftCompanionRef>,
) -> NativePrefixIntent {
    NativePrefixIntent {
        kind: NativePrefixKind::Capture,
        directory: native.owner.directory().to_owned(),
        id: "msg_prefix_original".into(),
        item_ids: vec!["queued1".into()],
        delivery: None,
        companions,
        native_context_ids: Vec::new(),
        files: Vec::new(),
        edit: None,
    }
}

fn prefix_operation(native: QueueNativeSource, intent: NativePrefixIntent) -> QueueOperation {
    let request = QueueJson::new(serde_json::to_vec(&intent).expect("internal native intent"))
        .expect("bounded intent");
    QueueOperation::prefix(
        QueueOperationId::new("prefix_original").expect("local operation"),
        captured_owner(&native),
        captured_config(),
        MessageId::from(intent.id),
        native,
        request,
    )
    .expect("typed native prefix capture")
}

fn retain_operation_capture(
    journal: &mut AttachmentJournal,
    snapshot: &mut JournalSnapshot,
    operation: &QueueOperationId,
    name: &str,
    bytes: &[u8],
    kind: QueueObjectKind,
    label: u128,
) -> (QueueObjectId, QueueVerifiedCapture) {
    let capture = QueueCaptureBytes::new(kind, bytes.to_vec(), &[blob(label)])
        .expect("bounded source capture");
    let id = QueueObjectId::new(name).expect("local capture object");
    let object = QueueObject::planned(
        id.clone(),
        operation.clone(),
        kind,
        1,
        capture.reference().clone(),
    )
    .expect("capture plan");
    let mut queue = std::mem::take(&mut snapshot.queue);
    assert!(
        queue
            .admit_object(object, |candidate| {
                let mut view = JournalView::from(&*snapshot);
                view.queue = candidate;
                view.validate()
            })
            .is_ok(),
        "unrelated owners remain in the capture preflight"
    );
    snapshot.queue = queue;
    save(journal, snapshot);
    let hold = snapshot
        .queue
        .object(&id)
        .expect("object")
        .hold(0)
        .expect("chunk hold");
    let receipt = journal
        .put_queue(&hold, capture.bytes())
        .expect("durable exact bytes");
    snapshot
        .queue
        .record_chunk(&hold, &receipt)
        .expect("retain durable receipt");
    save(journal, snapshot);
    let proof = journal
        .verify_queue(&snapshot.queue.object(&id).expect("object").verification())
        .expect("worker hashes original bytes");
    (id, proof)
}

#[test]
fn compound_prefix_publication_binds_whole_native_and_intent_and_rolls_back_refusal() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let native = native_text();
    let intent = prefix_intent(&native, Vec::new());
    let source = PrefixSourceCapture {
        version: 1,
        composer: CompanionCapture {
            native: native.clone(),
            references: Vec::new(),
        },
        intent: intent.clone(),
    };
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, prefix_operation(native, intent));
    let operation = snapshot.queue.operations()[0].id().clone();
    let mut wrong = source.clone();
    wrong.composer.native.draft.text.push_str(" replacement");
    let (_, wrong_proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &operation,
        "wrong-prefix-source",
        &serde_json::to_vec(&wrong).expect("different compound"),
        QueueObjectKind::NativeCompanion,
        301,
    );
    let before = serde_json::to_vec(&snapshot.queue).expect("original source metadata");
    assert!(
        snapshot
            .queue
            .publish_prefix_source(&operation, &wrong_proof, |_| Ok(()))
            .is_err(),
        "a whole worker proof for different native bytes supplies no publication authority"
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged source"),
        before
    );
    let (object, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &operation,
        "prefix-source",
        &serde_json::to_vec(&source).expect("exact compound"),
        QueueObjectKind::NativeCompanion,
        302,
    );
    assert!(
        snapshot.queue.publish_source(&operation, &proof).is_err(),
        "native-only publication cannot weaken the compound contract"
    );
    let before = serde_json::to_vec(&snapshot.queue).expect("inline ownership");
    assert!(
        snapshot
            .queue
            .publish_prefix_source(&operation, &proof, |_| Err(JournalError::Invalid(
                "capacity refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("restored source and object progress"),
        before
    );
    snapshot
        .queue
        .publish_prefix_source(&operation, &proof, |_| Ok(()))
        .expect("exact complete native compound publication");
    assert!(
        matches!(snapshot.queue.operation(&operation).expect("prefix").source(), Some(QueueSource::Companion(id)) if id == &object)
    );
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) = AttachmentJournal::open(&scratch.journal()).expect("restart");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("capturing prefix survives restart");
    };
    assert!(matches!(
        restored
            .queue
            .operation(&operation)
            .expect("original operation")
            .action(),
        QueueAction::Prefix {
            phase: QueuePrefixPhase::Capturing,
            ..
        }
    ));
    assert!(
        restored.queue.byte_authority().acknowledgements.is_empty(),
        "prefix sources cannot authorize take acknowledgement"
    );
}

#[test]
fn prefix_real_request_requires_published_original_capture_and_is_immutable() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let native = native_text();
    let intent = prefix_intent(&native, Vec::new());
    let source = PrefixSourceCapture {
        version: 1,
        composer: CompanionCapture {
            native: native.clone(),
            references: Vec::new(),
        },
        intent: intent.clone(),
    };
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, prefix_operation(native, intent));
    let operation = snapshot.queue.operations()[0].id().clone();
    let (document, indexed) = attachment_payload(false);
    let pointer: serde_json::Value =
        serde_json::from_slice(&indexed.pointer().body().expect("strict pointer"))
            .expect("pointer JSON");
    let real = serde_json::json!({"format":"payload-v1","directory":"/project","id":"msg_prefix_original", "itemIds":["queued1"],"composer":pointer});
    let request = queue_json(real.clone());
    assert!(
        snapshot
            .queue
            .prepare_prefix_request(&operation, request.clone(), |_| Ok(()))
            .is_err(),
        "inline sources cannot prepare requests"
    );
    let (_, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &operation,
        "request-source",
        &serde_json::to_vec(&source).expect("compound"),
        QueueObjectKind::NativeCompanion,
        311,
    );
    snapshot
        .queue
        .publish_prefix_source(&operation, &proof, |_| Ok(()))
        .expect("publish compound source");
    assert!(
        snapshot
            .queue
            .prepare_prefix_request(&operation, request.clone(), |_| Ok(()))
            .is_err(),
        "unowned composer bytes cannot enter the request"
    );
    let (_, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &operation,
        "request-composer",
        document.bytes(),
        QueueObjectKind::Payload,
        312,
    );
    snapshot
        .queue
        .record_verified(&proof)
        .expect("retain verified composer");
    let before = serde_json::to_vec(&snapshot.queue).expect("original internal intent");
    assert!(
        snapshot
            .queue
            .prepare_prefix_request(&operation, request.clone(), |_| Err(JournalError::Invalid(
                "capacity refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged intent"),
        before
    );
    for (field, value) in [
        ("directory", serde_json::json!("/elsewhere")),
        ("id", serde_json::json!("msg_other")),
        ("itemIds", serde_json::json!(["other"])),
        ("delivery", serde_json::json!("steer")),
        ("extra", serde_json::json!(true)),
    ] {
        let mut changed = real.clone();
        changed[field] = value;
        assert!(
            snapshot
                .queue
                .prepare_prefix_request(&operation, queue_json(changed), |_| Ok(()))
                .is_err(),
            "original capture fields are immutable"
        );
    }
    snapshot
        .queue
        .prepare_prefix_request(&operation, request.clone(), |_| Ok(()))
        .expect("strict original real request");
    let mut changed_pointer = real.clone();
    changed_pointer["composer"]["payloadManifest"]["chunks"][0]["requestId"] =
        serde_json::json!("other_upload");
    assert!(
        snapshot
            .queue
            .prepare_prefix_request(&operation, queue_json(changed_pointer), |_| Ok(()))
            .is_err(),
        "upload labels cannot change after preparation"
    );
    let duplicate = String::from_utf8(request.bytes().to_vec())
        .expect("UTF-8")
        .replace(
            "\"encoding\":\"utf-8-json\"",
            "\"encoding\":\"utf-8-json\",\"encoding\":\"utf-8-json\"",
        );
    assert!(
        validate_real_prefix_request(
            &QueueJson::new(duplicate.into_bytes()).expect("JSON"),
            snapshot
                .queue
                .operation(&operation)
                .expect("prefix")
                .owner(),
            "msg_prefix_original"
        )
        .is_err(),
        "nested duplicate pointer fields are refused"
    );
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("restart with real request");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("prepared capture");
    };
    let QueueAction::Prefix { request: saved, .. } = restored
        .queue
        .operation(&operation)
        .expect("prefix")
        .action()
    else {
        panic!("prefix");
    };
    assert_eq!(
        saved, &request,
        "preview and reservation recovery use the exact saved real body"
    );
}

fn acknowledged_edit(journal: &mut AttachmentJournal) -> TakeFixture {
    acknowledged_edit_with_contexts(journal, &[])
}

fn acknowledged_edit_with_contexts(
    journal: &mut AttachmentJournal,
    context_ids: &[&str],
) -> TakeFixture {
    let mut fixture = take_fixture_with_contexts(journal, true, b"original", context_ids);
    fixture
        .snapshot
        .queue
        .record_take_transfer(
            &fixture.id,
            fixture.transfer.clone(),
            &fixture.snapshot.attachments,
            |_| Ok(()),
        )
        .expect("retained original transfer");
    fixture
        .snapshot
        .queue
        .prepare_take_destination(
            &fixture.id,
            fixture.destination.clone(),
            fixture.acknowledge.clone(),
            &fixture.snapshot.attachments,
            |_| Ok(()),
        )
        .expect("original local edit destination");
    save(journal, &fixture.snapshot);
    let permit = journal
        .authorize_queue_acknowledgement(&fixture.id, std::slice::from_ref(&fixture.proof))
        .expect("worker verified original files");
    let mut receipt = fixture.transfer.receipt.value().expect("original receipt");
    receipt["state"] = serde_json::json!("acknowledged");
    fixture
        .snapshot
        .queue
        .record_take_acknowledged(&permit, queue_json(receipt), |_| Ok(()))
        .expect("authoritative original take acknowledgement");
    save(journal, &fixture.snapshot);
    fixture
}

/// Shared worker regression setup, using genuine materialization, index/file
/// proof, acknowledgement and the production atomic pending-edit admission.
pub(crate) fn pending_consumer_edit_fixture(
    journal: &mut AttachmentJournal,
) -> (JournalSnapshot, QueueConsumerEditBinding, QueueCaptureBytes) {
    let mut fixture = acknowledged_edit(journal);
    let id =
        QueueOperationId::new("pending_edit_consumer").expect("stable consuming Send identity");
    let (registration, pending, operation, capture) =
        pending_consumer_edit_plan(&mut fixture.snapshot, id.clone());
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    queue
        .admit_composer_operation_from_capture(
            operation,
            &registration,
            Some(pending),
            Vec::new(),
            |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            },
        )
        .expect("pending edit, consumer and original occurrence claims admit atomically");
    fixture.snapshot.queue = queue;
    let binding = fixture
        .snapshot
        .queue
        .consumer_edit_binding(&id)
        .expect("live exact consuming edit authority");
    save(journal, &fixture.snapshot);
    (fixture.snapshot, binding, capture)
}

fn pending_consumer_edit_plan(
    snapshot: &mut JournalSnapshot,
    id: QueueOperationId,
) -> (
    Arc<CapturedComposerSource>,
    QueueObject,
    QueueOperation,
    QueueCaptureBytes,
) {
    let native = native_text();
    let references = snapshot.queue.draft_companions(&native.owner);
    let bytes = crate::composer_document::literal_submission_edit_for_test(
        &native.owner,
        &references,
        2,
        7,
    );
    let capture = QueueCaptureBytes::new(QueueObjectKind::NativeCompanion, bytes, &[blob(990)])
        .expect("exact bounded editing bytes");
    let object_id = QueueObjectId::new(&format!("take_original-{}-edit", id.as_str()))
        .expect("original Take owns edit object");
    let edit = ComposerEditRef {
        operation: references[0].operation.clone(),
        object: object_id.clone(),
        generation: 2,
        revision: 7,
        byte_len: capture.reference().total_bytes(),
        sha256: *capture.reference().sha256(),
        companions: references.clone(),
    };
    let pending = QueueObject::planned(
        object_id,
        edit.operation.clone(),
        QueueObjectKind::NativeCompanion,
        2,
        capture.reference().clone(),
    )
    .expect("pending edit plan");
    let registration = snapshot
        .queue
        .capture_composer_source(
            id.clone(),
            native.owner.clone(),
            references.clone(),
            None,
            Vec::new(),
        )
        .expect("capture genuinely current acknowledged companions");
    let mut operation = QueueOperation::local_send_with_edit(
        id.clone(),
        captured_owner(&native),
        captured_config(),
        MessageId::from("msg_pending_edit_consumer"),
        CompanionCapture { native, references },
        None,
        Vec::new(),
        Vec::new(),
        Some(edit),
    )
    .expect("one final immutable consuming source");
    operation
        .begin_composer_preparation(2, 7)
        .expect("pristine pending-edit marker");
    (registration, pending, operation, capture)
}

#[test]
fn captured_pending_edit_admission_after_clear_is_atomic_and_deleted_retention_grants_no_writes() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private acknowledged source");
    let mut fixture = acknowledged_edit(&mut journal);
    let id = QueueOperationId::new("captured_after_clear").expect("original operation identity");
    let (registration, pending, operation, _) =
        pending_consumer_edit_plan(&mut fixture.snapshot, id.clone());
    let owner = native_text().owner;
    fixture
        .snapshot
        .queue
        .clear_composer_document(&owner, |_| Ok(()))
        .expect("Clear affects current input only");
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("unchanged original ledger");
    let mut queue = fixture.snapshot.queue.clone();
    assert!(
        queue
            .admit_composer_operation_from_capture(
                operation.clone(),
                &registration,
                Some(pending.clone()),
                Vec::new(),
                |_| Err(JournalError::Invalid("bounded full-snapshot refusal"))
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&queue).expect("refused candidate"),
        before,
        "pending version, object, consumer and irreversible Take claim roll back together"
    );
    assert!(matches!(
        queue
            .operation(&fixture.id)
            .expect("original Take")
            .take_source_use(),
        Some(QueueTakeSourceUse::Unused)
    ));
    queue
        .admit_composer_operation_from_capture(
            operation,
            &registration,
            Some(pending),
            Vec::new(),
            |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            },
        )
        .expect("captured authority survives Clear without restoring draft membership");
    fixture.snapshot.queue = queue;
    let binding = fixture
        .snapshot
        .queue
        .consumer_edit_binding(&id)
        .expect("exact live binding");
    let hold = fixture
        .snapshot
        .queue
        .object(&binding.edit().object)
        .expect("pending plan")
        .hold(0)
        .expect("unwritten chunk");
    assert!(
        fixture
            .snapshot
            .queue
            .byte_authority()
            .permits_consumer_edit(&binding)
    );
    assert!(
        fixture.snapshot.queue.composer_edit(&owner).is_none(),
        "pending capture is never current publication"
    );
    let immutable = fixture
        .snapshot
        .queue
        .operation(&id)
        .expect("original consumer")
        .compound_source_bytes()
        .expect("sealed bytes");
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    queue
        .delete_local_sends(
            owner.runtime(),
            &[SessionId::from("ses_queue")],
            |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            },
        )
        .expect("canonical Delete accepts exact unfinished edit retention");
    fixture.snapshot.queue = queue;
    assert!(fixture.snapshot.queue.local_send_deleted(&id));
    assert!(
        !fixture
            .snapshot
            .queue
            .byte_authority()
            .permits_consumer_edit(&binding)
    );
    assert!(fixture.snapshot.queue.consumer_edit_binding(&id).is_err());
    assert_eq!(
        fixture
            .snapshot
            .queue
            .operation(&id)
            .expect("terminal retention")
            .compound_source_bytes()
            .expect("original source"),
        immutable
    );
    assert_eq!(
        fixture
            .snapshot
            .queue
            .object(&binding.edit().object)
            .expect("retained byte roots")
            .hold(0),
        Some(hold)
    );
    save(&mut journal, &fixture.snapshot);
    drop(journal);
    let (_, loaded) = AttachmentJournal::open(&scratch.journal())
        .expect("restart retains terminal pending custody");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("canonical terminal snapshot");
    };
    assert!(restored.queue.local_send_deleted(&id));
    assert!(
        restored
            .queue
            .operation(&binding.edit().operation)
            .is_some(),
        "original acknowledged Take survives consumer revocation"
    );
}

#[test]
fn pending_edit_is_valid_only_for_pristine_capturing_inline_or_exact_deleted_retention() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private pending capture");
    let (snapshot, binding, _) = pending_consumer_edit_fixture(&mut journal);
    for mutate in [0, 1, 2, 3] {
        let mut candidate = snapshot.queue.clone();
        let operation = candidate
            .operations
            .iter_mut()
            .find(|operation| operation.id() == binding.consumer())
            .expect("consumer");
        let preparation = operation.preparation.as_mut().expect("pristine marker");
        match mutate {
            0 => preparation.revision += 1,
            1 => preparation.generation += 1,
            2 => preparation
                .derived_context_ids
                .push("msg_unprepared".into()),
            _ => {
                if let QueueAction::Send { phase, .. } = &mut operation.action {
                    *phase = QueueSendPhase::Prepared {
                        frame: binding.edit().object.clone(),
                    };
                }
            }
        }
        let mut view = JournalView::from(&snapshot);
        view.queue = &candidate;
        assert!(
            view.validate().is_err(),
            "unverified edits cannot authorize later preparation or mismatched markers"
        );
        assert!(candidate.consumer_edit_binding(binding.consumer()).is_err());
    }
}

#[test]
fn all_three_captured_consumers_retain_pending_edits_but_cannot_publish_sources_before_edit_verification()
 {
    for kind in 0..3 {
        let scratch = Scratch::new();
        let (mut journal, _) =
            AttachmentJournal::open(&scratch.journal()).expect("private original source");
        let mut fixture = acknowledged_edit(&mut journal);
        let id = QueueOperationId::new("pending_edit_consumer")
            .expect("one original activation identity");
        let (registration, pending, send, _) =
            pending_consumer_edit_plan(&mut fixture.snapshot, id.clone());
        let native = native_text();
        let references = send.dependencies().to_vec();
        let edit = send.composer_edit().cloned();
        let mut operation = match kind {
            0 => send,
            1 => QueueOperation::enqueue_with_edit(
                id.clone(),
                captured_owner(&native),
                captured_config(),
                CompanionCapture {
                    native: native.clone(),
                    references: references.clone(),
                },
                Vec::new(),
                Vec::new(),
                edit.clone(),
            )
            .expect("typed pending enqueue"),
            _ => {
                let mut intent = prefix_intent(&native, references);
                intent.edit = edit.clone();
                QueueOperation::prefix(
                    id.clone(),
                    captured_owner(&native),
                    captured_config(),
                    MessageId::from(intent.id.clone()),
                    native.clone(),
                    QueueJson::new(serde_json::to_vec(&intent).expect("original prefix intent"))
                        .expect("bounded prefix intent"),
                )
                .expect("typed pending prefix")
            }
        };
        if kind != 0 {
            operation
                .begin_composer_preparation(2, 7)
                .expect("pristine exact marker");
        }
        fixture
            .snapshot
            .queue
            .clear_composer_document(&native.owner, |_| Ok(()))
            .expect("detach current input before admission");
        let mut queue = std::mem::take(&mut fixture.snapshot.queue);
        queue
            .admit_composer_operation_from_capture(
                operation,
                &registration,
                Some(pending),
                Vec::new(),
                |candidate| {
                    let mut view = JournalView::from(&fixture.snapshot);
                    view.queue = candidate;
                    view.validate()
                },
            )
            .expect("atomic captured admission does not require a surviving current draft");
        fixture.snapshot.queue = queue;
        let binding = fixture
            .snapshot
            .queue
            .consumer_edit_binding(&id)
            .expect("exact consumer-bound edit authority");
        let original = fixture
            .snapshot
            .queue
            .operation(&id)
            .expect("original consumer")
            .compound_source_bytes()
            .expect("immutable source bytes");
        let (_, proof) = retain_operation_capture(
            &mut journal,
            &mut fixture.snapshot,
            &id,
            "early-source",
            &original,
            QueueObjectKind::NativeCompanion,
            991,
        );
        let result = match kind {
            0 => fixture
                .snapshot
                .queue
                .publish_send_source(&id, &proof, |_| Ok(())),
            1 => fixture
                .snapshot
                .queue
                .publish_enqueue_source(&id, &proof, |_| Ok(())),
            _ => fixture
                .snapshot
                .queue
                .publish_prefix_source(&id, &proof, |_| Ok(())),
        };
        assert!(
            result.is_err(),
            "even a genuine whole-source proof cannot bypass unfinished exact edit custody"
        );
        assert!(matches!(
            fixture
                .snapshot
                .queue
                .operation(&id)
                .expect("retained consumer")
                .source(),
            Some(QueueSource::Inline(_))
        ));
        assert!(
            !fixture
                .snapshot
                .queue
                .object(&binding.edit().object)
                .expect("pending edit remains rooted")
                .verified()
        );
        assert!(
            fixture
                .snapshot
                .queue
                .composer_edit(&native.owner)
                .is_none(),
            "current publication remains strict"
        );
    }
}

#[test]
fn captured_admission_uses_current_take_claims_after_capture_and_clear_without_resealing_sources() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private original source");
    let mut fixture =
        acknowledged_edit_with_contexts(&mut journal, &["msg_original_one", "msg_original_two"]);
    let id = QueueOperationId::new("later_captured_consumer").expect("stable later activation");
    let (registration, pending, operation, _) =
        pending_consumer_edit_plan(&mut fixture.snapshot, id.clone());
    let source = operation
        .compound_source_bytes()
        .expect("seal constructed once before admission");
    let transfer = serde_json::to_vec(
        &fixture
            .snapshot
            .queue
            .local_take(&fixture.id)
            .expect("original acknowledged source")
            .transfer,
    )
    .expect("original index and payload/file bindings");
    let native = native_text();
    let first = local_send_operation(native.clone(), operation.dependencies().to_vec());
    let first_id = first.id().clone();
    admit(&mut fixture.snapshot, first);
    assert_eq!(
        fixture
            .snapshot
            .queue
            .required_companion_rebinding_ids(&operation)
            .expect("current claim decides replacements"),
        2
    );
    fixture
        .snapshot
        .queue
        .clear_composer_document(&native.owner, |_| Ok(()))
        .expect("current Clear does not erase claims");
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    queue
        .admit_composer_operation_from_capture(
            operation,
            &registration,
            Some(pending),
            vec!["msg_rebound_one".into(), "msg_rebound_two".into()],
            |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            },
        )
        .expect("repeated occurrence is decided atomically against the current ledger");
    fixture.snapshot.queue = queue;
    assert!(
        matches!(fixture.snapshot.queue.operation(&fixture.id).expect("original Take").take_source_use(),
        Some(QueueTakeSourceUse::Claimed { consumer }) if consumer == &first_id)
    );
    let consumer = fixture
        .snapshot
        .queue
        .operation(&id)
        .expect("later independent consumer");
    assert_eq!(
        consumer
            .compound_source_bytes()
            .expect("same original source"),
        source
    );
    assert_eq!(
        fixture
            .snapshot
            .queue
            .source_context_ids(consumer)
            .expect("later occurrence identities"),
        ["msg_rebound_one", "msg_rebound_two"]
    );
    assert_eq!(
        serde_json::to_vec(
            &fixture
                .snapshot
                .queue
                .local_take(&fixture.id)
                .expect("same original Take")
                .transfer
        )
        .expect("unchanged original transfer"),
        transfer
    );
}

#[test]
fn full_journal_capacity_refusal_rolls_back_pending_edit_and_first_claim_together() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private original source");
    let mut fixture = acknowledged_edit(&mut journal);
    for name in ["ses_capacity_one", "ses_capacity_two"] {
        fixture.snapshot.drafts.push((
            DraftKey::new(
                RuntimeId::from("other"),
                "/project",
                DraftTarget::Session(SessionId::from(name)),
            )
            .expect("independent capacity owner"),
            Draft::new("\0".repeat(crate::mutation_journal::MAX_TEXT)),
        ));
    }
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("unrelated owners still fit before the candidate");
    let id = QueueOperationId::new("bounded_capture_admission").expect("stable refused activation");
    let (registration, pending, original, _) =
        pending_consumer_edit_plan(&mut fixture.snapshot, id.clone());
    let mut native = native_text();
    native.draft = Draft::new("\0".repeat(crate::mutation_journal::MAX_TEXT));
    let mut operation = QueueOperation::local_send_with_edit(
        id.clone(),
        captured_owner(&native),
        captured_config(),
        MessageId::from("msg_bounded_capture"),
        CompanionCapture {
            native,
            references: original.dependencies().to_vec(),
        },
        None,
        Vec::new(),
        Vec::new(),
        original.composer_edit().cloned(),
    )
    .expect("individually valid captured source");
    operation
        .begin_composer_preparation(2, 7)
        .expect("original marker");
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("all original owners");
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    assert!(
        queue
            .admit_composer_operation_from_capture(
                operation,
                &registration,
                Some(pending.clone()),
                Vec::new(),
                |candidate| {
                    let mut view = JournalView::from(&fixture.snapshot);
                    view.queue = candidate;
                    view.validate()
                }
            )
            .is_err(),
        "the full 16 MiB journal bound still refuses this candidate"
    );
    assert_eq!(
        serde_json::to_vec(&queue).expect("rolled back ledger"),
        before
    );
    assert!(queue.operation(&id).is_none() && queue.object(pending.id()).is_none());
    assert!(matches!(
        queue
            .operation(&fixture.id)
            .expect("original Take")
            .take_source_use(),
        Some(QueueTakeSourceUse::Unused)
    ));
}

#[test]
fn verified_capture_pin_preserves_e1_while_e2_publication_and_retirement_progress() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private original source");
    let mut fixture = acknowledged_edit(&mut journal);
    let owner = native_text().owner;
    let references = fixture.snapshot.queue.draft_companions(&owner);
    let mut pinned = None;
    let mut first = None;
    for revision in 1..=2 {
        let bytes = crate::composer_document::literal_submission_edit_for_test(
            &owner,
            &references,
            1,
            revision,
        );
        let capture = QueueCaptureBytes::new(
            QueueObjectKind::NativeCompanion,
            bytes,
            &[blob(800 + u128::from(revision))],
        )
        .expect("exact editing capture");
        let object = QueueObjectId::new(&format!("take_original-edit-{revision}"))
            .expect("immutable version identity");
        let edit = ComposerEditRef {
            operation: references[0].operation.clone(),
            object: object.clone(),
            generation: 1,
            revision,
            byte_len: capture.reference().total_bytes(),
            sha256: *capture.reference().sha256(),
            companions: references.clone(),
        };
        let planned = QueueObject::planned(
            object.clone(),
            edit.operation.clone(),
            QueueObjectKind::NativeCompanion,
            1,
            capture.reference().clone(),
        )
        .expect("ordinary current write plan");
        fixture
            .snapshot
            .queue
            .plan_composer_edit(&owner, edit.clone(), planned, |_| Ok(()))
            .expect("ordinary autosave planning");
        save(&mut journal, &fixture.snapshot);
        let hold = fixture
            .snapshot
            .queue
            .object(&object)
            .expect("ordinary plan")
            .hold(0)
            .expect("original ordinal");
        let receipt = journal
            .put_queue(&hold, capture.bytes())
            .expect("real ordinary editing put");
        fixture
            .snapshot
            .queue
            .record_chunk(&hold, &receipt)
            .expect("exact ordinary receipt");
        save(&mut journal, &fixture.snapshot);
        let proof = journal
            .verify_queue(
                &fixture
                    .snapshot
                    .queue
                    .object(&object)
                    .expect("complete plan")
                    .verification(),
            )
            .expect("whole edit verification");
        fixture
            .snapshot
            .queue
            .record_verified(&proof)
            .expect("real verified state");
        fixture
            .snapshot
            .queue
            .publish_composer_edit(&owner, &edit, |_| Ok(()))
            .expect("ordinary current publication progresses");
        save(&mut journal, &fixture.snapshot);
        if revision == 1 {
            pinned = Some(
                fixture
                    .snapshot
                    .queue
                    .capture_composer_source(
                        QueueOperationId::new("before_yield_capture").expect("activation"),
                        owner.clone(),
                        references.clone(),
                        Some(edit.clone()),
                        Vec::new(),
                    )
                    .expect("synchronous verified E1 protection"),
            );
            first = Some(edit);
        }
    }
    let first = first.expect("verified E1");
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    queue
        .retire_composer_edits(&owner, |candidate| {
            let mut view = JournalView::from(&fixture.snapshot);
            view.queue = candidate;
            view.validate()
        })
        .expect("E2 retirement succeeds while preserving protected E1");
    fixture.snapshot.queue = queue;
    assert_eq!(
        fixture
            .snapshot
            .queue
            .composer_edit(&owner)
            .expect("current autosave")
            .revision,
        2
    );
    assert!(
        fixture
            .snapshot
            .queue
            .object(&first.object)
            .is_some_and(|object| object.verified())
    );
    drop(pinned);
    fixture
        .snapshot
        .queue
        .retire_composer_edits(&owner, |_| Ok(()))
        .expect("unprotected obsolete E1 can enter normal named retirement");
    assert!(fixture.snapshot.queue.object(&first.object).is_none());
}

fn local_send_operation(
    native: QueueNativeSource,
    references: Vec<DraftCompanionRef>,
) -> QueueOperation {
    QueueOperation::local_send(
        QueueOperationId::new("local_send_original").expect("local ledger ID"),
        captured_owner(&native),
        captured_config(),
        MessageId::from("msg_local_original"),
        CompanionCapture { native, references },
        None,
        Vec::new(),
        Vec::new(),
    )
    .expect("typed original local send")
}

fn local_frame(native: &QueueNativeSource) -> Vec<u8> {
    use openchamber_native_transport::client::ServerClient;
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    use openchamber_native_transport::queue_frozen_send::{
        QueueFrozenSend, QueueFrozenSource, QueueLocalFrozenItem, QueueLocalSendInput,
    };
    use openchamber_native_transport::queue_manifest::{
        QueueLocalCapture, QueueLocalItem, QueuePromptParts,
    };

    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("synthetic endpoint"),
        Credential::ClientToken("synthetic-storage-test-token".into()),
    )
    .expect("captured test client");
    let capture = QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: native.draft.text.clone(),
            text: native.draft.text.clone(),
            files: Vec::new(),
            send_config: captured_config().restore(),
            prompt_parts: QueuePromptParts::default(),
        },
        Vec::new(),
        sha256,
    )
    .expect("genuine local capture");
    let sources = [QueueFrozenSource::Local(QueueLocalFrozenItem {
        capture: &capture,
        files: &[],
    })];
    QueueFrozenSend::capture_local(
        &client,
        QueueLocalSendInput {
            session: &SessionId::from("ses_queue"),
            directory: "/project",
            message_id: &MessageId::from("msg_local_original"),
            sources: &sources,
            selected_input: 0,
            delivery: None,
        },
        sha256,
    )
    .expect("real local frozen frame without any queue index")
    .into_bytes()
}

fn frame_with_original_companion(native: &QueueNativeSource) -> Vec<u8> {
    use openchamber_native_transport::client::ServerClient;
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    use openchamber_native_transport::queue_frozen_send::{
        QueueFrozenItem, QueueFrozenSend, QueueFrozenSource, QueueLocalFrozenItem,
        QueueLocalSendInput, QueuePreparedFile,
    };
    use openchamber_native_transport::queue_manifest::{
        QueueLocalCapture, QueueLocalItem, QueuePayloadFileSource, QueuePromptParts,
    };

    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("synthetic endpoint"),
        Credential::ClientToken("synthetic-storage-test-token".into()),
    )
    .expect("captured test client");
    let capture = QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: native.draft.text.clone(),
            text: native.draft.text.clone(),
            files: Vec::new(),
            send_config: captured_config().restore(),
            prompt_parts: QueuePromptParts::default(),
        },
        Vec::new(),
        sha256,
    )
    .expect("genuine native capture");
    let (original, indexed) = attachment_payload(true);
    let original = indexed
        .materialize(original.bytes().to_vec(), QueuePayloadUse::Enqueue, sha256)
        .expect("original take materialization");
    let files = [QueuePreparedFile::new(
        QueuePayloadFileSource::Manifest([0xab; 32]),
        8,
        sha256(b"original"),
        "file:///project/notes.txt",
    )
    .expect("original prepared file binding")];
    let sources = [
        QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &capture,
            files: &[],
        }),
        QueueFrozenSource::Queued(QueueFrozenItem {
            document: &original,
            indexed_payload: &indexed,
            files: &files,
        }),
    ];
    QueueFrozenSend::capture_local(
        &client,
        QueueLocalSendInput {
            session: &SessionId::from("ses_queue"),
            directory: "/project",
            message_id: &MessageId::from("msg_local_original"),
            sources: &sources,
            selected_input: 0,
            delivery: None,
        },
        sha256,
    )
    .expect("native source and genuine original companion")
    .into_bytes()
}

#[test]
fn local_send_has_its_own_verified_frame_lifecycle_and_no_acknowledgement_authority() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let native = native_text();
    let mut snapshot = JournalSnapshot::default();
    admit(
        &mut snapshot,
        local_send_operation(native.clone(), Vec::new()),
    );
    let operation = snapshot.queue.operations()[0].id().clone();
    let bytes = snapshot
        .queue
        .operation(&operation)
        .expect("local send")
        .compound_source_bytes()
        .expect("exact typed send source");
    let source: SendSourceCapture =
        serde_json::from_slice(&bytes).expect("lossless typed source schema");
    assert_eq!(source.composer.native.draft, native.draft);
    let (_, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &operation,
        "local-source",
        &bytes,
        QueueObjectKind::NativeCompanion,
        321,
    );
    snapshot
        .queue
        .publish_send_source(&operation, &proof, |_| Ok(()))
        .expect("whole compound source proof");
    let (frame, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &operation,
        "local-frame",
        &local_frame(&native),
        QueueObjectKind::FrozenSend,
        322,
    );
    assert!(
        snapshot
            .queue
            .prepare_local_send(&operation, frame.clone(), |_| Ok(()))
            .is_err(),
        "durable chunks require whole-frame verification"
    );
    snapshot
        .queue
        .record_verified(&proof)
        .expect("whole original outgoing frame");
    let before = serde_json::to_vec(&snapshot.queue).expect("capturing send");
    assert!(
        snapshot
            .queue
            .prepare_local_send(&operation, frame.clone(), |_| Err(JournalError::Invalid(
                "capacity refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged capturing send"),
        before
    );
    snapshot
        .queue
        .prepare_local_send(&operation, frame, |_| Ok(()))
        .expect("prepared original frame");
    let before = serde_json::to_vec(&snapshot.queue).expect("prepared send");
    assert!(
        snapshot
            .queue
            .mark_send_unknown(&operation, |_| Err(JournalError::Invalid(
                "capacity refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("still prepared"),
        before
    );
    snapshot
        .queue
        .mark_send_unknown(&operation, |_| Ok(()))
        .expect("unknown before dispatch");
    assert!(
        snapshot
            .queue
            .retire_operation(&operation, |_| Ok(()))
            .is_err(),
        "unknown admission retains all original bytes"
    );
    assert!(
        snapshot
            .queue
            .begin_local_send_retirement(&operation, Vec::new(), |_| Ok(()))
            .is_err(),
        "unknown admission cannot create a local cleanup continuation"
    );
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) = AttachmentJournal::open(&scratch.journal()).expect("restart");
    let LoadedJournal::Present(mut snapshot) = loaded else {
        panic!("local send survived");
    };
    assert!(matches!(
        snapshot
            .queue
            .operation(&operation)
            .expect("local send")
            .action(),
        QueueAction::Send {
            phase: QueueSendPhase::Unknown { .. },
            ..
        }
    ));
    assert!(
        snapshot.queue.byte_authority().acknowledgements.is_empty(),
        "local send grants no take proof"
    );
    assert!(
        snapshot
            .queue
            .record_send_admitted(&operation, &MessageId::from("msg_wrong"), |_| Ok(()))
            .is_err()
    );
    let before = serde_json::to_vec(&snapshot.queue).expect("unknown send");
    assert!(
        snapshot
            .queue
            .record_send_admitted(&operation, &MessageId::from("msg_local_original"), |_| Err(
                JournalError::Invalid("capacity refused")
            ))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged unknown send"),
        before
    );
    snapshot
        .queue
        .record_send_admitted(&operation, &MessageId::from("msg_local_original"), |_| {
            Ok(())
        })
        .expect("authoritative original prompt admission");
    assert!(
        snapshot
            .queue
            .mark_send_unknown(&operation, |_| Ok(()))
            .is_err(),
        "admitted local send cannot reopen"
    );
    let before = serde_json::to_vec(&snapshot.queue).expect("admitted send before cleanup");
    assert!(
        snapshot
            .queue
            .begin_local_send_retirement(&operation, Vec::new(), |_| Err(JournalError::Invalid(
                "save refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("refused cleanup remains unchanged"),
        before
    );
    snapshot
        .queue
        .begin_local_send_retirement(&operation, Vec::new(), |_| Ok(()))
        .expect("durable cleanup plan");
    assert!(
        snapshot
            .queue
            .finish_local_send_retirement(&operation, |_| Ok(()))
            .is_err(),
        "the admitted consumer must leave first"
    );
    snapshot
        .queue
        .retire_operation(&operation, |_| Ok(()))
        .expect("terminal retirement intent");
    assert!(snapshot.queue.operations().is_empty());
    let encoded = serde_json::to_vec(&snapshot.queue).expect("cleanup outlives consumer");
    let mut restored: QueueStorage =
        serde_json::from_slice(&encoded).expect("cleanup survives metadata restart");
    assert!(restored.local_send_retirement(&operation).is_some());
    assert!(
        restored.byte_authority().acknowledgements.is_empty(),
        "cleanup gains no take acknowledgement authority"
    );
    assert!(
        restored.byte_authority().native_roots.is_empty(),
        "cleanup is not its own live consumer"
    );
    assert!(
        restored
            .admit_operation(local_send_operation(native, Vec::new()), |_| Ok(()))
            .is_err(),
        "a pending cleanup identity cannot be reused"
    );
    let before = serde_json::to_vec(&restored).expect("retained cleanup");
    assert!(
        restored
            .finish_local_send_retirement(&operation, |_| Err(JournalError::Invalid(
                "save refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&restored).expect("refused finish keeps cleanup"),
        before
    );
    restored
        .finish_local_send_retirement(&operation, |_| Ok(()))
        .expect("independent deletion intents keep physical release ownership");
    assert!(restored.local_send_retirements().is_empty());
    assert!(
        !snapshot
            .queue
            .retired_blobs()
            .expect("retirement intents")
            .is_empty(),
        "retirement records intent without deleting files"
    );
}

#[test]
fn original_companion_bytes_remain_rooted_after_global_consumption_and_through_send_retirement() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut fixture = acknowledged_edit(&mut journal);
    let native = native_text();
    let references = fixture.snapshot.queue.draft_companions(&native.owner);
    let original = references[0].clone();
    assert!(
        fixture
            .snapshot
            .queue
            .consume_draft_companions(&native.owner, &references, |_| Ok(()))
            .is_err(),
        "consumption requires a durable consuming source"
    );
    let operation = local_send_operation(native.clone(), references.clone());
    let id = operation.id().clone();
    admit(&mut fixture.snapshot, operation);
    let bytes = fixture
        .snapshot
        .queue
        .operation(&id)
        .expect("send")
        .compound_source_bytes()
        .expect("source and dependency order");
    let (_, proof) = retain_operation_capture(
        &mut journal,
        &mut fixture.snapshot,
        &id,
        "dependent-source",
        &bytes,
        QueueObjectKind::NativeCompanion,
        331,
    );
    fixture
        .snapshot
        .queue
        .publish_send_source(&id, &proof, |_| Ok(()))
        .expect("sealed compound source");
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("newer input remains owned");
    fixture
        .snapshot
        .queue
        .consume_composer_capture(
            &id,
            &native.owner,
            &references,
            None,
            ComposerConsumption::PreserveCurrent,
            |_| Ok(()),
        )
        .expect("a newer revision can retain its own companion roots");
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("preserved current capture"),
        before
    );
    fixture
        .snapshot
        .queue
        .consume_draft_companions(&native.owner, &references, |_| Ok(()))
        .expect("atomic matching capture consumption");
    assert!(
        fixture
            .snapshot
            .queue
            .draft_companions(&native.owner)
            .is_empty()
    );
    assert!(
        fixture.snapshot.queue.object(&original.companion).is_some(),
        "original expanded source remains an owned object"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .retire_operation(&original.operation, |_| Ok(()))
            .is_err(),
        "the consuming operation roots its original take"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .references_native(&AttachmentId::from(fixture.local_blob.as_str())),
        "original file bytes stay protected"
    );
    let (frame, proof) = retain_operation_capture(
        &mut journal,
        &mut fixture.snapshot,
        &id,
        "dependent-frame",
        &frame_with_original_companion(&native),
        QueueObjectKind::FrozenSend,
        332,
    );
    fixture
        .snapshot
        .queue
        .record_verified(&proof)
        .expect("original compound outgoing frame");
    fixture
        .snapshot
        .queue
        .prepare_local_send(&id, frame, |_| Ok(()))
        .expect("prepared complete dependency send");
    fixture
        .snapshot
        .queue
        .mark_send_unknown(&id, |_| Ok(()))
        .expect("unknown send keeps original dependency roots");
    save(&mut journal, &fixture.snapshot);
    assert!(
        journal
            .read_queue(
                &fixture
                    .snapshot
                    .queue
                    .object(&original.companion)
                    .expect("original companion")
                    .reference()
                    .chunks()[0]
                    .descriptor()
                    .expect("original chunk")
            )
            .is_ok()
    );
    drop(journal);
    let (mut journal, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("restart with consumed references");
    let LoadedJournal::Present(mut snapshot) = loaded else {
        panic!("original dependencies persisted");
    };
    assert_eq!(
        snapshot.queue.operation(&id).expect("send").dependencies(),
        references
    );
    assert!(snapshot.queue.object(&original.companion).is_some());
    snapshot
        .queue
        .record_send_admitted(&id, &MessageId::from("msg_local_original"), |_| Ok(()))
        .expect("original prompt admission");
    assert!(
        snapshot
            .queue
            .begin_local_send_retirement(&id, Vec::new(), |_| Ok(()))
            .is_err(),
        "cleanup cannot omit captured companion bytes"
    );
    let files = vec![QueueNativeFileDescriptor {
        id: fixture.local_blob.as_str().to_owned(),
        byte_len: 8,
        sha256: sha256(b"original"),
    }];
    snapshot
        .queue
        .begin_local_send_retirement(&id, files.clone(), |_| Ok(()))
        .expect("exact original cleanup owners");
    snapshot
        .queue
        .begin_local_send_retirement(&id, files, |_| Ok(()))
        .expect("repeated cleanup is idempotent");
    assert_eq!(snapshot.queue.local_send_retirements().len(), 1);
    save(&mut journal, &snapshot);
    assert!(
        snapshot
            .queue
            .retire_operation(&original.operation, |_| Ok(()))
            .is_err(),
        "admission alone does not release consuming-operation dependencies"
    );
    snapshot
        .queue
        .retire_operation(&id, |_| Ok(()))
        .expect("terminal consuming operation retirement");
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) = AttachmentJournal::open(&scratch.journal())
        .expect("restart between consumer and original take retirement");
    let LoadedJournal::Present(mut snapshot) = loaded else {
        panic!("cleanup continuation persisted");
    };
    assert!(
        snapshot.queue.local_send_retirement(&id).is_some(),
        "consumer removal cannot lose the original dependency cleanup obligation"
    );
    assert!(
        snapshot
            .queue
            .finish_local_send_retirement(&id, |_| Ok(()))
            .is_err(),
        "unowned acknowledged take still requires cleanup"
    );
    snapshot
        .queue
        .retire_operation(&original.operation, |_| Ok(()))
        .expect("original take retires only after consuming owner retires");
    snapshot
        .queue
        .finish_local_send_retirement(&id, |_| Ok(()))
        .expect("all original queue ownership cleanup is planned");
    assert!(
        scratch.blob(&fixture.local_blob).exists(),
        "metadata retirement never deletes original attachment bytes"
    );
}

#[test]
fn dependency_admission_rejects_duplicates_foreign_owners_and_another_consuming_operation() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut fixture = acknowledged_edit(&mut journal);
    let native = native_text();
    let references = fixture.snapshot.queue.draft_companions(&native.owner);
    let duplicate = local_send_operation(
        native.clone(),
        vec![references[0].clone(), references[0].clone()],
    );
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("original takes");
    assert!(
        fixture
            .snapshot
            .queue
            .admit_operation(duplicate, |_| Ok(()))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("unchanged original takes"),
        before
    );
    let mut foreign = local_send_operation(native.clone(), references.clone());
    foreign.owner.runtime = "another-runtime".into();
    assert!(
        fixture
            .snapshot
            .queue
            .admit_operation(foreign, |_| Ok(()))
            .is_err()
    );
    let mut self_reference = local_send_operation(native.clone(), references.clone());
    self_reference.id = references[0].operation.clone();
    assert!(
        fixture
            .snapshot
            .queue
            .validate_dependencies(&self_reference)
            .is_err()
    );
    admit(
        &mut fixture.snapshot,
        local_send_operation(native.clone(), references.clone()),
    );
    let mut competing = local_send_operation(native, references);
    competing.id = QueueOperationId::new("competing_local_send").expect("independent operation ID");
    assert!(
        fixture
            .snapshot
            .queue
            .admit_operation(competing, |_| Ok(()))
            .is_err(),
        "one original companion cannot feed two live sends"
    );
    let first = DraftCompanionRef {
        operation: QueueOperationId::new("first_take").expect("ID"),
        companion: QueueObjectId::new("first").expect("object"),
    };
    let second = DraftCompanionRef {
        operation: QueueOperationId::new("second_take").expect("ID"),
        companion: QueueObjectId::new("second").expect("object"),
    };
    assert!(
        !ordered_references(&[first.clone(), second.clone()], &[second, first]),
        "capture order is the original composer append order"
    );
}

#[test]
fn native_compound_files_keep_original_catalog_descriptors_and_source_document_roots() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut fixture = acknowledged_edit(&mut journal);
    let mut native = native_text();
    native.draft.content.files.push(DraftFile {
        id: AttachmentId::from(fixture.local_blob.as_str()),
        name: Some("notes.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(8),
        source_document_id: Some(AttachmentId::from("original-document")),
        content: DraftFileContent::Prepared,
    });
    let file = QueueNativeFileDescriptor {
        id: fixture.local_blob.as_str().to_owned(),
        byte_len: 8,
        sha256: sha256(b"original"),
    };
    let construct = |file| {
        QueueOperation::local_send(
            QueueOperationId::new("local_files_original").expect("ID"),
            captured_owner(&native),
            captured_config(),
            MessageId::from("msg_files_original"),
            CompanionCapture {
                native: native.clone(),
                references: Vec::new(),
            },
            None,
            Vec::new(),
            vec![file],
        )
        .expect("captured native files")
    };
    let mut wrong = file.clone();
    wrong.sha256 = sha256(b"tampered");
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("original queue metadata");
    let mut queue = std::mem::take(&mut fixture.snapshot.queue);
    assert!(
        queue
            .admit_operation(construct(wrong), |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            })
            .is_err(),
        "matching lengths cannot substitute another native byte descriptor"
    );
    fixture.snapshot.queue = queue;
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("unchanged queue metadata"),
        before
    );
    let operation = construct(file);
    let id = operation.id().clone();
    admit(&mut fixture.snapshot, operation);
    let bytes = fixture
        .snapshot
        .queue
        .operation(&id)
        .expect("send")
        .compound_source_bytes()
        .expect("original native descriptor capture");
    let (_, proof) = retain_operation_capture(
        &mut journal,
        &mut fixture.snapshot,
        &id,
        "native-files-source",
        &bytes,
        QueueObjectKind::NativeCompanion,
        341,
    );
    fixture
        .snapshot
        .queue
        .publish_send_source(&id, &proof, |_| Ok(()))
        .expect("whole original capture proof");
    assert!(
        fixture
            .snapshot
            .queue
            .references_native(&AttachmentId::from("original-document")),
        "source document groups remain native roots after publication"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .byte_authority()
            .native_roots
            .contains(fixture.local_blob.as_str()),
        "worker authority protects original native file bytes"
    );
}

#[test]
fn strict_legacy_v8_prefix_and_absent_new_optional_fields_remain_readable() {
    let native = native_text();
    let (_, indexed) = attachment_payload(false);
    let pointer: serde_json::Value =
        serde_json::from_slice(&indexed.pointer().body().expect("pointer")).expect("JSON");
    let request = queue_json(
        serde_json::json!({"format":"payload-v1","directory":"/project","id":"msg_legacy_prefix","itemIds":["queued1"],"composer":pointer}),
    );
    let operation = QueueOperation::prefix(
        QueueOperationId::new("legacy_prefix").expect("ID"),
        captured_owner(&native),
        captured_config(),
        MessageId::from("msg_legacy_prefix"),
        native,
        request,
    )
    .expect("strict earlier v8 capturing prefix");
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation);
    let mut value = serde_json::to_value(&snapshot.queue).expect("legacy queue");
    let operation = value["operations"][0]
        .as_object_mut()
        .expect("operation object");
    operation.remove("source_intent");
    operation.remove("source_seal");
    operation.remove("preparation");
    snapshot.queue =
        serde_json::from_value(value).expect("new optional fields default for earlier v8");
    JournalView::from(&snapshot)
        .validate()
        .expect("strict legacy v8 capture remains valid");
    let mut malformed = prefix_intent(&native_text(), Vec::new());
    malformed.delivery = Some(Delivery::Queue);
    let request = QueueJson::new(serde_json::to_vec(&malformed).expect("intent")).expect("JSON");
    assert!(
        QueueOperation::prefix(
            QueueOperationId::new("invalid_prefix").expect("ID"),
            captured_owner(&native_text()),
            captured_config(),
            MessageId::from("msg_prefix_original"),
            native_text(),
            request
        )
        .is_err(),
        "internal prefix intent cannot widen real delivery semantics"
    );
}

fn preparation_input(
    native: &QueueNativeSource,
) -> openchamber_native_transport::queue_manifest::QueueLocalCapture {
    use openchamber_native_transport::queue_manifest::{
        QueueLocalCapture, QueueLocalItem, QueuePromptParts,
    };

    QueueLocalCapture::new(
        native.owner.directory(),
        QueueLocalItem {
            content: native.draft.text.clone(),
            text: native.draft.text.clone(),
            files: Vec::new(),
            send_config: captured_config().restore(),
            prompt_parts: QueuePromptParts::default(),
        },
        Vec::new(),
        sha256,
    )
    .expect("genuine original local input")
}

fn preparation_plan(
    operation: &QueueOperation,
    input: &openchamber_native_transport::queue_manifest::QueueLocalCapture,
    files: Vec<QueueDerivedFile>,
    contexts: Vec<String>,
) -> QueueComposerPreparation {
    QueueComposerPreparation::new(
        &operation.owner.draft_key().expect("captured owner"),
        *operation.source_sha256().expect("original source seal"),
        1,
        1,
        vec![QueuePreparationSource {
            identity: QueuePreparationSourceId::Source(0),
            byte_len: input.bytes().len() as u64,
            sha256: input.sha256(),
        }],
        files,
        contexts,
    )
}

fn derived_file(label: u128) -> QueueDerivedFile {
    QueueDerivedFile {
        source: QueuePreparationSourceId::Source(0),
        mention: QueuePreparationMention {
            start: 0,
            end: 8,
            text: "@doc.txt".into(),
        },
        server_path: "/project/doc.txt".into(),
        output_ordinal: 0,
        raw_source_byte_len: 7,
        raw_source_sha256: sha256(b"derived"),
        local_attachment_id: blob(label).as_str().to_owned(),
        blob: blob(label).as_str().to_owned(),
        preparation_generation: 1,
        byte_len: 7,
        sha256: sha256(b"derived"),
        name: "doc.txt".into(),
        mime_type: "text/plain".into(),
        document_group: Some("derived-document".into()),
        state: QueueDerivedFileState::Proposed,
    }
}

fn verify_preparation_input(
    input: &openchamber_native_transport::queue_manifest::QueueLocalCapture,
    preparation: &QueueComposerPreparation,
) -> Result<(), JournalError> {
    if preparation.sources.len() != 1
        || preparation.sources[0].identity != QueuePreparationSourceId::Source(0)
        || preparation.sources[0].byte_len != input.bytes().len() as u64
        || preparation.sources[0].sha256 != input.sha256()
    {
        return Err(JournalError::Invalid("input byte verification refused"));
    }
    for file in &preparation.files {
        let units: Vec<_> = input
            .item()
            .content
            .encode_utf16()
            .skip(file.mention.start as usize)
            .take((file.mention.end - file.mention.start) as usize)
            .collect();
        if String::from_utf16(&units).ok().as_deref() != Some(file.mention.text.as_str()) {
            return Err(JournalError::Invalid(
                "input mention byte verification refused",
            ));
        }
    }
    Ok(())
}

#[test]
fn preparation_admission_preserves_original_seal_and_rolls_back_holds_contexts_and_refusal() {
    let mut native = native_text();
    native.draft = Draft::new("@doc.txt $skill");
    let input = preparation_input(&native);
    let mut snapshot = JournalSnapshot::default();
    admit(
        &mut snapshot,
        local_send_operation(native.clone(), Vec::new()),
    );
    let id = snapshot.queue.operations()[0].id().clone();
    let original = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .compound_source_bytes()
        .expect("original bytes");
    let proposal = preparation_plan(
        snapshot.queue.operation(&id).expect("send"),
        &input,
        vec![derived_file(501)],
        vec!["msg_derived_skill".into()],
    );
    let before = serde_json::to_vec(&snapshot.queue).expect("original ledger");
    let mut holds = Vec::new();
    assert!(
        snapshot
            .queue
            .plan_composer_preparation(
                &id,
                proposal.clone(),
                &snapshot.attachments,
                &mut holds,
                |_, _| Err(JournalError::Invalid("source verification refused")),
                |_, _| Ok(())
            )
            .is_err()
    );
    assert!(
        holds.is_empty(),
        "failed byte verification grants no put authority"
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged source"),
        before
    );
    assert!(
        snapshot
            .queue
            .plan_composer_preparation(
                &id,
                proposal.clone(),
                &snapshot.attachments,
                &mut holds,
                |_, plan| verify_preparation_input(&input, plan),
                |candidate, candidate_holds| {
                    assert_eq!(
                        candidate_holds.len(),
                        1,
                        "canonical snapshot includes the exact hold"
                    );
                    assert_eq!(
                        candidate
                            .composer_context_ids(&id)
                            .expect("effective order"),
                        ["msg_derived_skill"]
                    );
                    Err(JournalError::Invalid("snapshot write refused"))
                }
            )
            .is_err()
    );
    assert!(
        holds.is_empty(),
        "failed snapshot admission rolls back every hold"
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged source"),
        before
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            proposal.clone(),
            &snapshot.attachments,
            &mut holds,
            |_, plan| verify_preparation_input(&input, plan),
            |candidate, candidate_holds| {
                candidate.validate(
                    &snapshot.attachments,
                    candidate_holds,
                    &[],
                    &mut HashSet::new(),
                )
            },
        )
        .expect("complete operation-owned plan");
    snapshot.preparations = holds;
    let operation = snapshot.queue.operation(&id).expect("original send");
    assert_eq!(
        operation
            .compound_source_bytes()
            .expect("unchanged original capture"),
        original
    );
    let Some(QueueSourceIntent::Send(intent)) = &operation.source_intent else {
        panic!("original source intent");
    };
    assert!(
        intent.native_context_ids.is_empty(),
        "derived context does not change original count"
    );
    assert!(
        intent.files.is_empty(),
        "derived files do not change original file order"
    );
    let QueueAction::Send { context_ids, .. } = operation.action() else {
        panic!("send");
    };
    assert_eq!(
        context_ids,
        &["msg_derived_skill"],
        "capturing Send tracks the full context order"
    );
    assert!(
        snapshot
            .queue
            .references_native(&AttachmentId::from(blob(501).as_str()))
    );
    assert!(
        snapshot
            .queue
            .references_native(&AttachmentId::from("derived-document"))
    );
    assert!(
        snapshot
            .queue
            .byte_authority()
            .native_roots
            .contains(blob(501).as_str())
    );
    let encoded = serde_json::to_vec(&snapshot.queue).expect("durable metadata representation");
    let restored: QueueStorage = serde_json::from_slice(&encoded).expect("preparation round trip");
    restored
        .validate(
            &snapshot.attachments,
            &snapshot.preparations,
            &[],
            &mut HashSet::new(),
        )
        .expect("exact holds survive reload");
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            proposal,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, _| Ok(()),
            |_, _| Ok(()),
        )
        .expect("an exact duplicate plan is idempotent");
    assert_eq!(
        snapshot.preparations.len(),
        1,
        "an exact retry cannot allocate another hold"
    );
}

#[test]
fn all_capturing_composer_operations_reserve_derived_context_ids_against_other_contexts_and_prompts()
 {
    for first_kind in 0..3 {
        let native = native_text();
        let input = preparation_input(&native);
        let first = match first_kind {
            0 => local_send_operation(native.clone(), Vec::new()),
            1 => prefix_operation(native.clone(), prefix_intent(&native, Vec::new())),
            _ => QueueOperation::enqueue_with_edit(
                QueueOperationId::new("compound_enqueue").expect("ID"),
                captured_owner(&native),
                captured_config(),
                CompanionCapture {
                    native: native.clone(),
                    references: Vec::new(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .expect("compound enqueue"),
        };
        let id = first.id().clone();
        let mut snapshot = JournalSnapshot::default();
        admit(&mut snapshot, first);
        let proposal = preparation_plan(
            snapshot.queue.operation(&id).expect("captured operation"),
            &input,
            Vec::new(),
            vec!["msg_reserved_skill".into()],
        );
        snapshot
            .queue
            .plan_composer_preparation(
                &id,
                proposal,
                &snapshot.attachments,
                &mut snapshot.preparations,
                |_, plan| verify_preparation_input(&input, plan),
                |_, _| Ok(()),
            )
            .expect("capture reserves derived ID");
        let competing = QueueOperation::local_send(
            QueueOperationId::new("competing_send").expect("ID"),
            captured_owner(&native),
            captured_config(),
            MessageId::from("msg_reserved_skill"),
            CompanionCapture {
                native: native.clone(),
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("competing prompt");
        assert!(
            snapshot
                .queue
                .admit_operation(competing, |_| Ok(()))
                .is_err(),
            "even a minimal preflight cannot admit a prompt alias"
        );
        let competing = QueueOperation::local_send(
            QueueOperationId::new("independent_send").expect("ID"),
            captured_owner(&native),
            captured_config(),
            MessageId::from("msg_independent"),
            CompanionCapture {
                native: native.clone(),
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("independent complete captured identity");
        admit(&mut snapshot, competing);
        let second = QueueOperationId::new("independent_send").expect("ID");
        let plan = preparation_plan(
            snapshot.queue.operation(&second).expect("second owner"),
            &input,
            Vec::new(),
            vec!["msg_reserved_skill".into()],
        );
        assert!(
            snapshot
                .queue
                .plan_composer_preparation(
                    &second,
                    plan,
                    &snapshot.attachments,
                    &mut snapshot.preparations,
                    |_, _| Ok(()),
                    |_, _| Ok(())
                )
                .is_err(),
            "derived context IDs reserve globally within their runtime"
        );
        let mut messages = HashSet::from([("runtime", "msg_reserved_skill")]);
        assert!(
            snapshot
                .queue
                .validate(
                    &snapshot.attachments,
                    &snapshot.preparations,
                    &[],
                    &mut messages
                )
                .is_err(),
            "another journal owner cannot reuse a capturing operation's context ID"
        );
        let mut foreign_directory = native.clone();
        foreign_directory.owner = DraftKey::new(
            RuntimeId::from("runtime"),
            "/another-project",
            DraftTarget::Session(SessionId::from("ses_queue")),
        )
        .expect("another directory in same runtime");
        let competitor = QueueOperation::local_send(
            QueueOperationId::new("other_directory_send").expect("ID"),
            captured_owner(&foreign_directory),
            captured_config(),
            MessageId::from("msg_reserved_skill"),
            CompanionCapture {
                native: foreign_directory,
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("cross-directory prompt");
        assert!(
            snapshot
                .queue
                .admit_operation(competitor, |_| Ok(()))
                .is_err(),
            "directory boundaries do not permit message aliases"
        );
        let mut foreign_runtime = native.clone();
        foreign_runtime.owner = DraftKey::new(
            RuntimeId::from("independent-runtime"),
            "/project",
            DraftTarget::Session(SessionId::from("ses_queue")),
        )
        .expect("independent runtime");
        let independent = QueueOperation::local_send(
            QueueOperationId::new("other_runtime_send").expect("ID"),
            captured_owner(&foreign_runtime),
            captured_config(),
            MessageId::from("msg_reserved_skill"),
            CompanionCapture {
                native: foreign_runtime,
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("runtime-scoped prompt");
        admit(&mut snapshot, independent);
    }
}

#[test]
fn derived_file_validation_rejects_changed_source_holds_catalog_descriptors_and_unedited_companions()
 {
    let mut native = native_text();
    native.draft = Draft::new("@doc.txt");
    let input = preparation_input(&native);
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, local_send_operation(native, Vec::new()));
    let id = snapshot.queue.operations()[0].id().clone();
    for invalid in 0..8 {
        let mut plan = preparation_plan(
            snapshot.queue.operation(&id).expect("send"),
            &input,
            vec![derived_file(510)],
            Vec::new(),
        );
        match invalid {
            0 => plan.source_sha256 = [9; 32],
            1 => {
                plan.owner = Owner::capture(
                    &DraftKey::new(
                        RuntimeId::from("another-runtime"),
                        "/project",
                        DraftTarget::Session(SessionId::from("ses_queue")),
                    )
                    .expect("foreign owner"),
                )
            }
            2 => {
                plan.sources[0].identity = QueuePreparationSourceId::Source(1);
                plan.files[0].source = QueuePreparationSourceId::Source(1);
            }
            3 => {
                plan.sources[0].identity = QueuePreparationSourceId::Literal(0);
                plan.files[0].source = QueuePreparationSourceId::Literal(0);
            }
            4 => plan.files[0].preparation_generation = 2,
            5 => plan.files[0].blob = blob(511).as_str().to_owned(),
            6 => plan.files[0].server_path = "../doc.txt".into(),
            _ => {
                plan.files[0].raw_source_byte_len =
                    crate::attachment_files::MAX_INLINE_ATTACHMENT_BYTES as u64 + 1
            }
        }
        assert!(
            snapshot
                .queue
                .plan_composer_preparation(
                    &id,
                    plan,
                    &snapshot.attachments,
                    &mut snapshot.preparations,
                    |_, _| Ok(()),
                    |_, _| Ok(())
                )
                .is_err(),
            "invalid persisted provenance cannot acquire holds"
        );
        assert!(snapshot.preparations.is_empty());
    }
    let plan = preparation_plan(
        snapshot.queue.operation(&id).expect("send"),
        &input,
        vec![derived_file(510)],
        Vec::new(),
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            plan,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("valid proposal");
    let mut wrong_hold = snapshot.preparations.clone();
    wrong_hold[0].generation += 1;
    assert!(
        snapshot
            .queue
            .validate(&snapshot.attachments, &wrong_hold, &[], &mut HashSet::new())
            .is_err()
    );
    assert!(
        snapshot
            .queue
            .validate(&snapshot.attachments, &[], &[], &mut HashSet::new())
            .is_err()
    );
    let file = &snapshot
        .queue
        .operation(&id)
        .expect("send")
        .composer_preparation()
        .expect("plan")
        .files[0];
    snapshot.attachments.insert(
        AttachmentId::from(file.local_attachment_id.clone()),
        PreparedAttachment {
            blob: file.descriptor().expect("descriptor"),
            staging: None,
        },
    );
    assert!(
        snapshot
            .queue
            .validate(
                &snapshot.attachments,
                &snapshot.preparations,
                &[],
                &mut HashSet::new()
            )
            .is_err(),
        "catalog custody cannot appear without the publication transition"
    );
    snapshot.queue.operations[0]
        .preparation
        .as_mut()
        .expect("plan")
        .files[0]
        .state = QueueDerivedFileState::Cataloged;
    snapshot.preparations.clear();
    snapshot
        .queue
        .validate(&snapshot.attachments, &[], &[], &mut HashSet::new())
        .expect("exact catalog publication state");
    snapshot
        .attachments
        .values_mut()
        .next()
        .expect("derived attachment")
        .blob = BlobDescriptor::new(blob(510), 7, sha256(b"changed"));
    assert!(
        snapshot
            .queue
            .validate(&snapshot.attachments, &[], &[], &mut HashSet::new())
            .is_err(),
        "equal length never substitutes different bytes"
    );
}

fn derived_capture(
    native: &QueueNativeSource,
    file: &QueueDerivedFile,
) -> openchamber_native_transport::queue_manifest::QueueLocalCapture {
    use openchamber_native_transport::queue_manifest::{
        QueueLocalCapture, QueueLocalFile, QueueLocalItem, QueuePromptParts,
    };

    QueueLocalCapture::new(
        native.owner.directory(),
        QueueLocalItem {
            content: native.draft.text.clone(),
            text: native.draft.text.clone(),
            files: vec![QueueLocalFile {
                name: file.name.clone(),
                mime_type: file.mime_type.clone(),
                byte_len: file.byte_len,
                sha256: file.sha256,
                description: None,
                mention: None,
            }],
            send_config: captured_config().restore(),
            prompt_parts: QueuePromptParts::default(),
        },
        Vec::new(),
        sha256,
    )
    .expect("genuine prepared local capture")
}

fn derived_frame(
    capture: &openchamber_native_transport::queue_manifest::QueueLocalCapture,
) -> Vec<u8> {
    use openchamber_native_transport::client::ServerClient;
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    use openchamber_native_transport::queue_frozen_send::{
        QueueFrozenSend, QueueFrozenSource, QueueLocalFrozenItem, QueueLocalSendInput,
        QueuePreparedFile,
    };

    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("synthetic endpoint"),
        Credential::ClientToken("synthetic-storage-test-token".into()),
    )
    .expect("captured test client");
    let file = &capture.item().files[0];
    let files = [QueuePreparedFile::new_local(
        0,
        file.byte_len,
        file.sha256,
        "file:///project/prepared-doc.txt",
    )
    .expect("exact prepared output target")];
    let sources = [QueueFrozenSource::Local(QueueLocalFrozenItem {
        capture,
        files: &files,
    })];
    QueueFrozenSend::capture_local(
        &client,
        QueueLocalSendInput {
            session: &SessionId::from("ses_queue"),
            directory: "/project",
            message_id: &MessageId::from("msg_local_original"),
            sources: &sources,
            selected_input: 0,
            delivery: None,
        },
        sha256,
    )
    .expect("real frozen frame includes derived output")
    .into_bytes()
}

#[test]
fn derived_output_custody_precedes_immutable_inputs_and_survives_send_cleanup_continuation() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut native = native_text();
    native.draft = Draft::new("@doc.txt");
    let input = preparation_input(&native);
    let file = derived_file(520);
    let prepared_input = derived_capture(&native, &file);
    let mut snapshot = JournalSnapshot::default();
    admit(
        &mut snapshot,
        local_send_operation(native.clone(), Vec::new()),
    );
    let id = snapshot.queue.operations()[0].id().clone();
    let original_bytes = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .compound_source_bytes()
        .expect("original capture");
    let proposal = preparation_plan(
        snapshot.queue.operation(&id).expect("send"),
        &input,
        vec![file.clone()],
        Vec::new(),
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            proposal,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("pre-put plan");
    let hold = snapshot.preparations[0].clone();
    assert_eq!(
        journal.put(&hold, b"derived"),
        Err(AttachmentFailure::Unauthorized),
        "unsaved metadata cannot put bytes"
    );
    let bytes = QueueCaptureBytes::new(
        QueueObjectKind::NativeCompanion,
        prepared_input.bytes().to_vec(),
        &[blob(521)],
    )
    .expect("prepared input bytes");
    let premature = QueueObject::planned(
        QueueObjectId::new("premature-inputs").expect("ID"),
        id.clone(),
        QueueObjectKind::NativeCompanion,
        1,
        bytes.reference().clone(),
    )
    .expect("input object plan");
    assert!(
        snapshot.queue.admit_object(premature, |_| Ok(())).is_err(),
        "input byte puts wait for catalog receipts"
    );
    save(&mut journal, &snapshot);
    let receipt = journal
        .put(&hold, b"derived")
        .expect("durable exact output put");
    let before = serde_json::to_vec(&snapshot.queue).expect("proposed plan");
    assert!(
        snapshot
            .queue
            .publish_composer_derived_file(
                &id,
                1,
                0,
                &receipt,
                &mut snapshot.attachments,
                &mut snapshot.preparations,
                |_, _, _| Err(JournalError::Invalid("catalog save refused"))
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged plan"),
        before
    );
    assert!(snapshot.attachments.is_empty());
    assert_eq!(
        snapshot.preparations,
        std::slice::from_ref(&hold),
        "publication refusal restores exact put authority"
    );
    let wrong = BlobReceipt {
        descriptor: BlobDescriptor::new(blob(520), 7, sha256(b"changed")),
        commit: SpoolCommit::Durable,
    };
    assert!(
        snapshot
            .queue
            .publish_composer_derived_file(
                &id,
                1,
                0,
                &wrong,
                &mut snapshot.attachments,
                &mut snapshot.preparations,
                |_, _, _| Ok(())
            )
            .is_err()
    );
    snapshot
        .queue
        .publish_composer_derived_file(
            &id,
            1,
            0,
            &receipt,
            &mut snapshot.attachments,
            &mut snapshot.preparations,
            |candidate, catalog, holds| {
                candidate.validate(catalog, holds, &[], &mut HashSet::new())
            },
        )
        .expect("exact durable catalog publication");
    save(&mut journal, &snapshot);
    let (_, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &id,
        "derived-source",
        &original_bytes,
        QueueObjectKind::NativeCompanion,
        522,
    );
    snapshot
        .queue
        .publish_send_source(&id, &proof, |_| Ok(()))
        .expect("original source remains unchanged");
    let (inputs, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &id,
        "derived-inputs",
        prepared_input.bytes(),
        QueueObjectKind::NativeCompanion,
        523,
    );
    snapshot
        .queue
        .record_verified(&proof)
        .expect("committed input bytes verified under owner lock");
    let before = serde_json::to_vec(&snapshot.queue).expect("unbound inputs");
    assert!(
        snapshot
            .queue
            .seal_composer_inputs(
                &id,
                1,
                1,
                inputs.clone(),
                &snapshot.attachments,
                &snapshot.preparations,
                |_, _, _| Ok(()),
                |_| Err(JournalError::Invalid("input publication refused"))
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged binding"),
        before
    );
    snapshot
        .queue
        .seal_composer_inputs(
            &id,
            1,
            1,
            inputs.clone(),
            &snapshot.attachments,
            &snapshot.preparations,
            |_, plan, object| {
                if plan.files[0].sha256 != file.sha256
                    || object.reference.sha256() != &prepared_input.sha256()
                {
                    return Err(JournalError::Invalid(
                        "prepared input byte verification refused",
                    ));
                }
                Ok(())
            },
            |_| Ok(()),
        )
        .expect("one immutable inputs binding");
    let sealed = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .composer_preparation()
        .expect("sealed inputs")
        .clone();
    assert!(
        snapshot
            .queue
            .extend_composer_preparation(
                &id,
                sealed,
                &snapshot.attachments,
                &mut snapshot.preparations,
                |_, _| Ok(()),
                |_, _| Ok(())
            )
            .is_err(),
        "sealed inputs prohibit every further derivation"
    );
    let (other, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &id,
        "other-inputs",
        input.bytes(),
        QueueObjectKind::NativeCompanion,
        524,
    );
    snapshot
        .queue
        .record_verified(&proof)
        .expect("unrelated verified bytes");
    assert!(
        snapshot
            .queue
            .seal_composer_inputs(
                &id,
                1,
                1,
                other,
                &snapshot.attachments,
                &snapshot.preparations,
                |_, _, _| Ok(()),
                |_| Ok(())
            )
            .is_err(),
        "even verified bytes cannot replace published inputs"
    );
    let (frame, proof) = retain_operation_capture(
        &mut journal,
        &mut snapshot,
        &id,
        "derived-frame",
        &derived_frame(&prepared_input),
        QueueObjectKind::FrozenSend,
        525,
    );
    snapshot
        .queue
        .record_verified(&proof)
        .expect("complete real frame");
    snapshot
        .queue
        .prepare_local_send(&id, frame, |_| Ok(()))
        .expect("prepared derived send");
    snapshot
        .queue
        .record_send_admitted(&id, &MessageId::from("msg_local_original"), |_| Ok(()))
        .expect("original message admitted");
    assert!(
        snapshot
            .queue
            .begin_local_send_retirement(&id, Vec::new(), |_| Ok(()))
            .is_err(),
        "cleanup cannot omit new outputs"
    );
    let files = snapshot
        .queue
        .composer_file_descriptors(&id)
        .expect("all captured file obligations");
    assert_eq!(files.len(), 1);
    snapshot
        .queue
        .begin_local_send_retirement(&id, files, |_| Ok(()))
        .expect("exact derived cleanup continuation");
    save(&mut journal, &snapshot);
    drop(journal);
    let (mut journal, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("reload published inputs and cleanup");
    let LoadedJournal::Present(mut snapshot) = loaded else {
        panic!("durable published preparation");
    };
    assert_eq!(
        snapshot
            .queue
            .operation(&id)
            .expect("send")
            .composer_preparation()
            .expect("preparation")
            .inputs_object(),
        Some(&inputs)
    );
    assert!(
        snapshot
            .queue
            .references_native(&AttachmentId::from(blob(520).as_str()))
    );
    snapshot
        .queue
        .retire_operation(&id, |_| Ok(()))
        .expect("terminal consumer retirement");
    assert!(
        snapshot.queue.local_send_retirement(&id).is_some(),
        "new outputs remain in cleanup continuation"
    );
    assert!(
        !snapshot
            .queue
            .references_native(&AttachmentId::from(blob(520).as_str())),
        "cleanup never self-roots output bytes"
    );
    assert!(snapshot.queue.byte_authority().native_roots.is_empty());
    let attachment = snapshot
        .attachments
        .remove(blob(520).as_str())
        .expect("catalog output");
    snapshot.retired_blobs.push(*attachment.blob.id());
    save(&mut journal, &snapshot);
    assert_eq!(
        journal.release(attachment.blob.id()),
        Ok(SpoolCommit::Durable),
        "durable unreference permits output deletion"
    );
    snapshot
        .queue
        .finish_local_send_retirement(&id, |_| Ok(()))
        .expect("cleanup continuation completes independently");
}

#[test]
fn preparation_extensions_preserve_first_success_and_append_complete_bundles_with_rollback() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut native = native_text();
    native.draft = Draft::new("@doc.txt @later.txt");
    let input = preparation_input(&native);
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, local_send_operation(native, Vec::new()));
    let id = snapshot.queue.operations()[0].id().clone();
    let base = preparation_plan(
        snapshot.queue.operation(&id).expect("send"),
        &input,
        Vec::new(),
        Vec::new(),
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            base,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("durable base authority");
    let mut first = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .composer_preparation()
        .expect("base")
        .clone();
    first.files.push(derived_file(530));
    snapshot
        .queue
        .extend_composer_preparation(
            &id,
            first,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("first whole mention bundle");
    save(&mut journal, &snapshot);
    let receipt = journal
        .put(&snapshot.preparations[0], b"derived")
        .expect("first successful output");
    snapshot
        .queue
        .publish_composer_derived_file(
            &id,
            1,
            0,
            &receipt,
            &mut snapshot.attachments,
            &mut snapshot.preparations,
            |_, _, _| Ok(()),
        )
        .expect("first success cataloged");
    save(&mut journal, &snapshot);
    let before = serde_json::to_vec(&snapshot.queue).expect("first successful bundle retained");
    let mut next = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .composer_preparation()
        .expect("base")
        .clone();
    let mut file = derived_file(531);
    file.mention = QueuePreparationMention {
        start: 9,
        end: 19,
        text: "@later.txt".into(),
    };
    file.server_path = "/project/later.txt".into();
    file.name = "later.txt".into();
    file.document_group = Some("later-document".into());
    next.files.push(file.clone());
    next.derived_context_ids
        .push("msg_later_instruction".into());
    assert!(
        snapshot
            .queue
            .extend_composer_preparation(
                &id,
                next.clone(),
                &snapshot.attachments,
                &mut snapshot.preparations,
                |_, plan| verify_preparation_input(&input, plan),
                |_, _| Err(JournalError::Invalid("later plan save refused"))
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("first success unchanged"),
        before
    );
    assert!(
        snapshot.preparations.is_empty(),
        "failed extension rolls back only its proposed holds"
    );
    assert_eq!(
        snapshot.attachments.len(),
        1,
        "later failure cannot discard first catalog publication"
    );
    for mutation in 0..4 {
        let mut changed = next.clone();
        match mutation {
            0 => changed.files[0].local_attachment_id = blob(532).as_str().to_owned(),
            1 => changed.files[0].state = QueueDerivedFileState::Proposed,
            2 => changed.files[0].raw_source_sha256 = [8; 32],
            _ => changed.revision += 1,
        }
        assert!(
            snapshot
                .queue
                .extend_composer_preparation(
                    &id,
                    changed,
                    &snapshot.attachments,
                    &mut snapshot.preparations,
                    |_, _| Ok(()),
                    |_, _| Ok(())
                )
                .is_err(),
            "old IDs, states, raw bytes and revision remain immutable"
        );
    }
    snapshot
        .queue
        .extend_composer_preparation(
            &id,
            next.clone(),
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("later bundle append");
    snapshot
        .queue
        .extend_composer_preparation(
            &id,
            next.clone(),
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("exact later-bundle retry");
    assert_eq!(
        snapshot.preparations.len(),
        1,
        "exact retry never reallocates output labels"
    );
    assert_eq!(snapshot.preparations[0].blob, blob(531));
    let mut extra_sibling = next.clone();
    file.local_attachment_id = blob(532).as_str().to_owned();
    file.blob = file.local_attachment_id.clone();
    file.output_ordinal = 1;
    extra_sibling.files.push(file);
    assert!(
        snapshot
            .queue
            .extend_composer_preparation(
                &id,
                extra_sibling,
                &snapshot.attachments,
                &mut snapshot.preparations,
                |_, _| Ok(()),
                |_, _| Ok(())
            )
            .is_err(),
        "a recorded whole bundle cannot gain omitted siblings later"
    );
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("restart after later-file failure");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("partial preparation persists");
    };
    let plan = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .composer_preparation()
        .expect("partial plan");
    assert!(plan.files[0].state == QueueDerivedFileState::Cataloged);
    assert!(plan.files[1].state == QueueDerivedFileState::Proposed);
    assert!(
        plan.inputs_object().is_none(),
        "partial preparation grants no final inputs"
    );
    assert_eq!(snapshot.attachments.len(), 1);
    assert!(
        snapshot
            .queue
            .references_native(&AttachmentId::from(blob(530).as_str()))
    );
    assert!(
        snapshot
            .queue
            .references_native(&AttachmentId::from(blob(531).as_str()))
    );
}

#[test]
fn a_document_bundle_binds_raw_source_identity_and_all_output_ordinals_before_any_put() {
    let mut native = native_text();
    native.draft = Draft::new("@doc.txt");
    let input = preparation_input(&native);
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, local_send_operation(native, Vec::new()));
    let id = snapshot.queue.operations()[0].id().clone();
    for mismatch in 0..4 {
        let first = derived_file(540);
        let mut second = derived_file(541);
        second.output_ordinal = 1;
        match mismatch {
            0 => second.raw_source_sha256 = sha256(b"different raw source"),
            1 => second.raw_source_byte_len += 1,
            2 => second.server_path = "/project/replacement.txt".into(),
            _ => second.output_ordinal = 2,
        }
        let plan = preparation_plan(
            snapshot.queue.operation(&id).expect("send"),
            &input,
            vec![first, second],
            Vec::new(),
        );
        assert!(
            snapshot
                .queue
                .plan_composer_preparation(
                    &id,
                    plan,
                    &snapshot.attachments,
                    &mut snapshot.preparations,
                    |_, _| Ok(()),
                    |_, _| Ok(())
                )
                .is_err(),
            "all outputs bind one unchanged raw read and complete order"
        );
        assert!(snapshot.preparations.is_empty());
    }
}

#[test]
fn preparation_keeps_native_context_ids_immutable_and_inserts_derived_ids_in_preparation_order() {
    use openchamber_native_core::draft_content::DraftContext;
    use openchamber_native_core::ids::ContextItemId;
    use openchamber_native_transport::queue_manifest::{
        QueueLocalCapture, QueueLocalItem, QueuePromptContext, QueuePromptParts,
    };

    let mut native = native_text();
    native.draft.content.context.push(DraftContext {
        id: ContextItemId::from("native-context"),
        text: "Original source context".into(),
        description: None,
        metadata_json: None,
        instructions: Some("Original source instructions".into()),
    });
    let native_ids = vec![
        "msg_native_instructions".to_owned(),
        "msg_native_context".to_owned(),
    ];
    let input = QueueLocalCapture::new(
        native.owner.directory(),
        QueueLocalItem {
            content: native.draft.text.clone(),
            text: native.draft.text.clone(),
            files: Vec::new(),
            send_config: captured_config().restore(),
            prompt_parts: QueuePromptParts {
                context: vec![
                    QueuePromptContext {
                        id: None,
                        text: "Original source instructions".into(),
                        description: None,
                        metadata: None,
                    },
                    QueuePromptContext {
                        id: None,
                        text: "Original source context".into(),
                        description: None,
                        metadata: None,
                    },
                ],
                ..QueuePromptParts::default()
            },
        },
        native_ids.iter().cloned().map(MessageId::from).collect(),
        sha256,
    )
    .expect("real original context capture");
    let operation = QueueOperation::local_send(
        QueueOperationId::new("native_context_send").expect("ID"),
        captured_owner(&native),
        captured_config(),
        MessageId::from("msg_context_prompt"),
        CompanionCapture {
            native,
            references: Vec::new(),
        },
        None,
        native_ids.clone(),
        Vec::new(),
    )
    .expect("native capture");
    let id = operation.id().clone();
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation);
    let original = snapshot
        .queue
        .operation(&id)
        .expect("send")
        .compound_source_bytes()
        .expect("exact original bytes");
    let plan = preparation_plan(
        snapshot.queue.operation(&id).expect("send"),
        &input,
        Vec::new(),
        vec!["msg_skill_first".into(), "msg_skill_second".into()],
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            plan,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("derived instructions owner");
    assert_eq!(
        snapshot
            .queue
            .composer_context_ids(&id)
            .expect("full context order"),
        [
            "msg_native_instructions",
            "msg_native_context",
            "msg_skill_first",
            "msg_skill_second"
        ]
    );
    let operation = snapshot.queue.operation(&id).expect("send");
    assert_eq!(
        operation
            .compound_source_bytes()
            .expect("original source stays sealed"),
        original
    );
    let Some(QueueSourceIntent::Send(intent)) = &operation.source_intent else {
        panic!("native send capture");
    };
    assert_eq!(
        intent.native_context_ids, native_ids,
        "captured original context count and order never grow"
    );
}

fn preserved_consumer_fixture(
    journal: &mut AttachmentJournal,
    contexts: &[&str],
    prefix: bool,
) -> (TakeFixture, QueueOperationId) {
    let mut fixture = acknowledged_edit_with_contexts(journal, contexts);
    let native = native_text();
    let references = fixture.snapshot.queue.draft_companions(&native.owner);
    let first = if prefix {
        prefix_operation(native.clone(), prefix_intent(&native, references.clone()))
    } else {
        QueueOperation::enqueue_with_edit(
            QueueOperationId::new("preserved_first_enqueue").expect("first operation ID"),
            captured_owner(&native),
            captured_config(),
            CompanionCapture {
                native: native.clone(),
                references: references.clone(),
            },
            Vec::new(),
            Vec::new(),
            None,
        )
        .expect("first genuine companion consumer")
    };
    let id = first.id().clone();
    admit(&mut fixture.snapshot, first);
    let source = fixture
        .snapshot
        .queue
        .operation(&id)
        .expect("first consumer")
        .compound_source_bytes()
        .expect("immutable original capture");
    let (_, proof) = retain_operation_capture(
        journal,
        &mut fixture.snapshot,
        &id,
        "preserved-first-source",
        &source,
        QueueObjectKind::NativeCompanion,
        601,
    );
    if prefix {
        fixture
            .snapshot
            .queue
            .publish_prefix_source(&id, &proof, |_| Ok(()))
            .expect("published original prefix source");
    } else {
        fixture
            .snapshot
            .queue
            .publish_enqueue_source(&id, &proof, |_| Ok(()))
            .expect("published original enqueue source");
    }
    fixture.snapshot.drafts.push((
        native.owner.clone(),
        Draft::new("Newer typing while the first consumer prepares"),
    ));
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("current companion ownership");
    fixture
        .snapshot
        .queue
        .consume_composer_capture(
            &id,
            &native.owner,
            &references,
            None,
            ComposerConsumption::PreserveCurrent,
            |candidate| {
                candidate.validate(
                    &fixture.snapshot.attachments,
                    &fixture.snapshot.preparations,
                    &[],
                    &mut HashSet::new(),
                )
            },
        )
        .expect("whole newer composer intentionally remains available");
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("preserved complete ledger"),
        before
    );
    assert_eq!(
        fixture.snapshot.queue.draft_companions(&native.owner),
        references
    );
    if !prefix {
        use openchamber_native_transport::queue_manifest::{
            QueueAttachmentDescriptor, QueuePayloadItem,
        };

        let (original, indexed) = attachment_payload_with_contexts(true, contexts);
        let original = indexed
            .materialize(original.bytes().to_vec(), QueuePayloadUse::Enqueue, sha256)
            .expect("original sealed companion");
        let mut item = original.item().clone();
        let QueuePayloadItem::Manifest(manifest) = &mut item else {
            panic!("original manifest fixture");
        };
        manifest.content = format!("{}\n\n{}", native.draft.text, manifest.content);
        manifest.text = format!("{}\n\n{}", native.draft.text, manifest.text);
        let document =
            QueuePayloadDocument::from_item("/project", &item, QueuePayloadUse::Enqueue, sha256)
                .expect("new consumer payload retains original file and context facts");
        let pointer = document
            .pointer(
                vec![QueueAttachmentDescriptor {
                    blob_id: [0xde; 32],
                    request_id: "preserved_payload_upload".into(),
                    attachment_id: "payload-0".into(),
                    filename: "payload-0.json".into(),
                    mime_type: "application/json".into(),
                    size: document.bytes().len() as u64,
                    sha256: document.sha256(),
                }],
                sha256,
            )
            .expect("real transport pointer binds the new complete payload");
        let (payload, proof) = retain_operation_capture(
            journal,
            &mut fixture.snapshot,
            &id,
            "preserved-first-payload",
            document.bytes(),
            QueueObjectKind::Payload,
            602,
        );
        fixture
            .snapshot
            .queue
            .record_verified(&proof)
            .expect("whole new consumer payload");
        let pointer: serde_json::Value =
            serde_json::from_slice(&pointer.body().expect("pointer body")).expect("pointer JSON");
        fixture
            .snapshot
            .queue
            .prepare_enqueue(
                &id,
                payload,
                queue_json(serde_json::json!({"directory":"/project","item":pointer})),
                |_| Ok(()),
            )
            .expect("complete first enqueue");
        fixture.snapshot.queue.record_enqueue_accepted(&id, queue_json(serde_json::json!({"receipt":{
            "version":1,"kind":"enqueue","requestId":id.as_str(),"sessionId":"ses_queue","directory":"/project",
            "requestHash":"ad".repeat(32),"acceptedAt":1,"itemId":"preserved_queued_item",
            "messageId":"msg_preserved_first_enqueue","contextIds":contexts
        },"state":"queued","revision":1,"attempt":0})), |_| Ok(())).expect("first accepted enqueue retains its original context IDs");
    }
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("complete preserved-current fixture");
    (fixture, id)
}

fn next_preserved_consumer(snapshot: &JournalSnapshot, enqueue: bool) -> QueueOperation {
    let (owner, draft) = snapshot
        .drafts
        .iter()
        .find(|(owner, _)| owner.runtime().as_str() == "runtime")
        .expect("newer current draft remains present");
    let native = QueueNativeSource {
        owner: owner.clone(),
        draft: draft.clone(),
    };
    let references = snapshot.queue.draft_companions(owner);
    let capture = CompanionCapture {
        native: native.clone(),
        references,
    };
    if enqueue {
        QueueOperation::enqueue_with_edit(
            QueueOperationId::new("preserved_second_enqueue").expect("independent enqueue ID"),
            captured_owner(&native),
            captured_config(),
            capture,
            Vec::new(),
            Vec::new(),
            None,
        )
        .expect("second captured enqueue")
    } else {
        QueueOperation::local_send(
            QueueOperationId::new("preserved_second_send").expect("independent Send ID"),
            captured_owner(&native),
            captured_config(),
            MessageId::from("msg_preserved_second_send"),
            capture,
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("second captured ordinary Send")
    }
}

fn admit_occurrence(snapshot: &mut JournalSnapshot, operation: QueueOperation, fresh: Vec<String>) {
    let mut queue = std::mem::take(&mut snapshot.queue);
    let result = queue.admit_composer_operation(operation, fresh, |candidate| {
        let mut view = JournalView::from(&*snapshot);
        view.queue = candidate;
        view.validate()
    });
    snapshot.queue = queue;
    result
        .map_err(|(error, _)| error)
        .expect("complete independent source occurrence passes canonical admission");
}

fn unrelated_context_consumer() -> QueueOperation {
    use openchamber_native_core::draft_content::DraftContext;
    use openchamber_native_core::ids::ContextItemId;

    let mut native = native_text();
    native.draft = Draft::new("Unrelated current operation");
    native.draft.content.context.push(DraftContext {
        id: ContextItemId::from("unrelated-source-context"),
        text: "Independent source context".into(),
        description: None,
        metadata_json: None,
        instructions: None,
    });
    QueueOperation::local_send(
        QueueOperationId::new("unrelated_context_consumer").expect("independent operation ID"),
        captured_owner(&native),
        captured_config(),
        MessageId::from("msg_unrelated_prompt"),
        CompanionCapture {
            native,
            references: Vec::new(),
        },
        None,
        vec!["msg_unrelated_context".into()],
        Vec::new(),
    )
    .expect("genuine unrelated native context")
}

#[test]
fn preserve_current_allows_independent_send_and_enqueue_occurrences_of_the_same_genuine_take() {
    for contexts in [
        &[][..],
        &["msg_original_take_first", "msg_original_take_second"][..],
    ] {
        for prefix in [false, true] {
            let scratch = Scratch::new();
            let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
            let (mut fixture, first) = preserved_consumer_fixture(&mut journal, contexts, prefix);
            let original_transfer = serde_json::to_vec(
                &fixture
                    .snapshot
                    .queue
                    .local_take(&fixture.id)
                    .expect("original acknowledged Take")
                    .transfer,
            )
            .expect("exact original file/index/context bindings");
            let unrelated = unrelated_context_consumer();
            let unrelated_id = unrelated.id().clone();
            admit(&mut fixture.snapshot, unrelated);
            for enqueue in [false, true] {
                let next = next_preserved_consumer(&fixture.snapshot, enqueue);
                let next_id = next.id().clone();
                assert_ne!(
                    next_id, first,
                    "new submission has a distinct operation identity"
                );
                let captured = next.dependencies().to_vec();
                assert_eq!(
                    captured.len(),
                    1,
                    "whole current still contains its genuine original Take"
                );
                let before = serde_json::to_vec(&fixture.snapshot.queue)
                    .expect("all live owners before second admission");
                assert!(
                    fixture
                        .snapshot
                        .queue
                        .admit_operation(next.clone(), |_| Ok(()))
                        .is_err(),
                    "generic first-use admission cannot bypass explicit repeat custody, even with zero contexts"
                );
                assert_eq!(
                    serde_json::to_vec(&fixture.snapshot.queue)
                        .expect("refused generic admission preserves all owners"),
                    before
                );
                assert_eq!(
                    fixture
                        .snapshot
                        .queue
                        .required_companion_rebinding_ids(&next)
                        .expect("exact replacement count"),
                    contexts.len()
                );
                let fresh = contexts
                    .iter()
                    .enumerate()
                    .map(|(index, _)| format!("msg_repeat_{}_{index}", usize::from(enqueue)))
                    .collect::<Vec<_>>();
                let original_source = next
                    .compound_source_bytes()
                    .expect("immutable second capture");
                admit_occurrence(&mut fixture.snapshot, next, fresh.clone());
                let admitted = fixture
                    .snapshot
                    .queue
                    .operation(&next_id)
                    .expect("independent consumer");
                assert_eq!(
                    admitted
                        .compound_source_bytes()
                        .expect("source remains original"),
                    original_source
                );
                let rows = admitted
                    .source_occurrences()
                    .expect("explicit read-only occurrence table");
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].context_rebinding.as_ref(),
                    Some(&fresh),
                    "an empty mapping still records repeat custody"
                );
                assert_eq!(
                    fixture
                        .snapshot
                        .queue
                        .companion_context_ids(&next_id, 1)
                        .expect("admitted working IDs"),
                    fresh
                );
                assert_eq!(
                    fixture.snapshot.queue.draft_companions(&source().owner),
                    captured,
                    "new admission does not prune the current document"
                );
                assert_eq!(
                    fixture.snapshot.drafts[0].1.text,
                    "Newer typing while the first consumer prepares"
                );
                assert_eq!(
                    fixture
                        .snapshot
                        .queue
                        .composer_context_ids(&unrelated_id)
                        .expect("unrelated context order"),
                    ["msg_unrelated_context"]
                );
                assert_eq!(
                    fixture
                        .snapshot
                        .queue
                        .composer_context_ids(&first)
                        .expect("first-use companion IDs"),
                    contexts
                );
            }
            assert_eq!(
                serde_json::to_vec(
                    &fixture
                        .snapshot
                        .queue
                        .local_take(&fixture.id)
                        .expect("original Take remains")
                        .transfer
                )
                .expect("unchanged original authority"),
                original_transfer
            );
            assert!(
                fixture
                    .snapshot
                    .queue
                    .references_native(&AttachmentId::from(fixture.local_blob.as_str()))
            );
            assert!(
                fixture
                    .snapshot
                    .queue
                    .operation(&first)
                    .expect("first consumer")
                    .source_occurrences()
                    .expect("first occurrence table")
                    .iter()
                    .all(|source| source.context_rebinding.is_none()),
                "first use retains the exact original IDs"
            );
            save(&mut journal, &fixture.snapshot);
            drop(journal);
            let (_, loaded) = AttachmentJournal::open(&scratch.journal())
                .expect("reload all simultaneous read-only occurrences");
            let LoadedJournal::Present(restored) = loaded else {
                panic!("durable independent occurrences");
            };
            for enqueue in [false, true] {
                let id = QueueOperationId::new(if enqueue {
                    "preserved_second_enqueue"
                } else {
                    "preserved_second_send"
                })
                .expect("consumer ID");
                let row = &restored
                    .queue
                    .operation(&id)
                    .expect("reloaded consumer")
                    .source_occurrences()
                    .expect("reloaded occurrence table")[0];
                assert!(
                    row.context_rebinding.is_some(),
                    "explicit empty repeat bindings survive serialization"
                );
                assert_eq!(
                    row.context_rebinding
                        .as_ref()
                        .expect("explicit repeated IDs")
                        .len(),
                    contexts.len()
                );
            }
        }
    }
}

#[test]
fn retiring_first_enqueue_keeps_original_ids_spent_and_admits_newer_current_with_fresh_ids() {
    let contexts = ["msg_original_take_first", "msg_original_take_second"];
    for enqueue in [false, true] {
        let scratch = Scratch::new();
        let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
        let (mut fixture, first) = preserved_consumer_fixture(&mut journal, &contexts, false);
        let original = serde_json::to_vec(
            &fixture
                .snapshot
                .queue
                .local_take(&fixture.id)
                .expect("original Take")
                .transfer,
        )
        .expect("sealed original transfer");
        let first_ids = fixture
            .snapshot
            .queue
            .composer_context_ids(&first)
            .expect("first accepted context IDs");
        assert_eq!(first_ids, contexts);
        let unrelated = unrelated_context_consumer();
        let unrelated_id = unrelated.id().clone();
        admit(&mut fixture.snapshot, unrelated);
        save(&mut journal, &fixture.snapshot);
        let mut queue = std::mem::take(&mut fixture.snapshot.queue);
        queue
            .retire_operation(&first, |candidate| {
                let mut view = JournalView::from(&fixture.snapshot);
                view.queue = candidate;
                view.validate()
            })
            .expect(
                "accepted first consumer can retire while whole current keeps the original Take",
            );
        fixture.snapshot.queue = queue;
        assert!(fixture.snapshot.queue.operation(&first).is_none());
        assert!(fixture.snapshot.queue.operation(&fixture.id).is_some());
        assert!(
            matches!(fixture.snapshot.queue.operation(&fixture.id).expect("original Take").take_source_use(),
            Some(QueueTakeSourceUse::Claimed { consumer }) if consumer == &first)
        );
        save(&mut journal, &fixture.snapshot);
        drop(journal);
        let (mut journal, loaded) = AttachmentJournal::open(&scratch.journal())
            .expect("reload irreversible claim after first consumer retirement");
        let LoadedJournal::Present(restored) = loaded else {
            panic!("durable original claim");
        };
        fixture.snapshot = *restored;
        let reserved_prompt = QueueOperation::local_send(
            QueueOperationId::new("spent_context_prompt").expect("new prompt ID"),
            captured_owner(&native_text()),
            captured_config(),
            MessageId::from(contexts[0]),
            CompanionCapture {
                native: native_text(),
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("otherwise independent prompt");
        assert!(
            fixture
                .snapshot
                .queue
                .admit_operation(reserved_prompt, |_| Ok(()))
                .is_err(),
            "retirement never frees original context IDs for unrelated prompts"
        );
        let reused_operation = QueueOperation::enqueue(
            first.clone(),
            captured_owner(&native_text()),
            captured_config(),
            native_text(),
        )
        .expect("otherwise valid fresh source");
        assert!(
            fixture
                .snapshot
                .queue
                .admit_operation(reused_operation, |_| Ok(()))
                .is_err(),
            "a spent first-consumer ID cannot impersonate its original claim"
        );
        let next = next_preserved_consumer(&fixture.snapshot, enqueue);
        let next_id = next.id().clone();
        let fresh = vec![
            "msg_after_retirement_first".into(),
            "msg_after_retirement_second".into(),
        ];
        assert_eq!(
            fixture
                .snapshot
                .queue
                .required_companion_rebinding_ids(&next)
                .expect("spent original source"),
            2
        );
        admit_occurrence(&mut fixture.snapshot, next, fresh.clone());
        assert_eq!(
            fixture
                .snapshot
                .queue
                .composer_context_ids(&next_id)
                .expect("independent working IDs"),
            fresh
        );
        assert_ne!(
            fixture
                .snapshot
                .queue
                .companion_context_ids(&next_id, 1)
                .expect("independent companion IDs"),
            first_ids
        );
        assert_eq!(
            fixture
                .snapshot
                .queue
                .composer_context_ids(&unrelated_id)
                .expect("unrelated context order"),
            ["msg_unrelated_context"]
        );
        assert_eq!(
            serde_json::to_vec(
                &fixture
                    .snapshot
                    .queue
                    .local_take(&fixture.id)
                    .expect("same genuine Take")
                    .transfer
            )
            .expect("same original proof inputs"),
            original
        );
        assert_eq!(
            fixture
                .snapshot
                .queue
                .operations()
                .iter()
                .filter(|operation| matches!(operation.action(), QueueAction::Take { .. }))
                .count(),
            1,
            "a second occurrence does not manufacture another Take or acknowledgement"
        );
        assert!(
            fixture
                .snapshot
                .queue
                .references_native(&AttachmentId::from(fixture.local_blob.as_str()))
        );
        assert_eq!(
            fixture
                .snapshot
                .queue
                .draft_companions(&source().owner)
                .len(),
            1,
            "whole newer current remains retained"
        );
        JournalView::from(&fixture.snapshot)
            .validate()
            .expect("canonical validation retains spent original IDs and distinct working IDs");
        let mut external = HashSet::from([("runtime", contexts[0])]);
        assert!(
            fixture
                .snapshot
                .queue
                .validate(
                    &fixture.snapshot.attachments,
                    &fixture.snapshot.preparations,
                    &[],
                    &mut external
                )
                .is_err(),
            "retired original IDs also remain reserved against other journal owners"
        );
        save(&mut journal, &fixture.snapshot);
        assert_eq!(
            journal.authorize_queue_acknowledgement(&next_id, std::slice::from_ref(&fixture.proof)),
            Err(AttachmentFailure::Unauthorized),
            "a read-only repeat grants no second acknowledgement authority"
        );
    }
}

#[test]
fn source_occurrence_admission_refusal_rolls_back_first_claims_tables_and_identity_reservations() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut fixture = acknowledged_edit_with_contexts(
        &mut journal,
        &["msg_original_first", "msg_original_second"],
    );
    let native = native_text();
    let first = local_send_operation(
        native.clone(),
        fixture.snapshot.queue.draft_companions(&native.owner),
    );
    let first_id = first.id().clone();
    let source = first
        .compound_source_bytes()
        .expect("original captured source");
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("unclaimed Take");
    let refused = fixture.snapshot.queue.admit_composer_operation(first.clone(), Vec::new(), |candidate| {
        candidate.validate(&fixture.snapshot.attachments, &fixture.snapshot.preparations, &[], &mut HashSet::new())?;
        assert!(matches!(candidate.operation(&fixture.id).expect("Take").take_source_use(), Some(QueueTakeSourceUse::Claimed { consumer }) if consumer == &first_id));
        Err(JournalError::Invalid("first claim snapshot refused"))
    });
    let Err((_, returned)) = refused else {
        panic!("injected refusal");
    };
    assert!(returned.source_occurrences().is_none());
    assert_eq!(
        returned
            .compound_source_bytes()
            .expect("returned original source"),
        source
    );
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("complete first-claim rollback"),
        before
    );
    assert!(matches!(
        fixture
            .snapshot
            .queue
            .operation(&fixture.id)
            .expect("Take")
            .take_source_use(),
        Some(QueueTakeSourceUse::Unused)
    ));
    assert_eq!(
        fixture
            .snapshot
            .queue
            .required_companion_rebinding_ids(&first)
            .expect("first claim still unused"),
        0
    );
    admit(&mut fixture.snapshot, first);
    fixture
        .snapshot
        .drafts
        .push((native.owner, Draft::new("Newer independent current")));
    let unrelated = unrelated_context_consumer();
    admit(&mut fixture.snapshot, unrelated);
    let second = next_preserved_consumer(&fixture.snapshot, false);
    let second_id = second.id().clone();
    let original = second
        .compound_source_bytes()
        .expect("exact repeated capture");
    let before =
        serde_json::to_vec(&fixture.snapshot.queue).expect("first claim and unrelated owner");
    for fresh in [
        vec!["msg_only_one".into()],
        vec!["msg_duplicate".into(), "msg_duplicate".into()],
        vec!["msg_original_first".into(), "msg_fresh".into()],
        vec!["msg_unrelated_context".into(), "msg_fresh".into()],
    ] {
        assert!(
            fixture
                .snapshot
                .queue
                .admit_composer_operation(second.clone(), fresh, |_| Ok(()))
                .is_err()
        );
        assert_eq!(
            serde_json::to_vec(&fixture.snapshot.queue)
                .expect("identity refusal preserves every owner"),
            before
        );
    }
    let fresh = vec![
        "msg_valid_repeat_first".into(),
        "msg_valid_repeat_second".into(),
    ];
    let refused =
        fixture
            .snapshot
            .queue
            .admit_composer_operation(second, fresh.clone(), |candidate| {
                candidate.validate(
                    &fixture.snapshot.attachments,
                    &fixture.snapshot.preparations,
                    &[],
                    &mut HashSet::new(),
                )?;
                assert_eq!(candidate.companion_context_ids(&second_id, 1)?, fresh);
                Err(JournalError::Invalid("repeated claim snapshot refused"))
            });
    let Err((_, returned)) = refused else {
        panic!("injected repeat refusal");
    };
    assert!(returned.source_occurrences().is_none());
    assert_eq!(
        returned
            .compound_source_bytes()
            .expect("returned immutable repeat source"),
        original
    );
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("complete repeat rollback"),
        before
    );
    admit_occurrence(&mut fixture.snapshot, *returned, fresh);
    let admitted = fixture
        .snapshot
        .queue
        .operation(&second_id)
        .expect("committed independent occurrence");
    assert!(
        fixture
            .snapshot
            .queue
            .required_companion_rebinding_ids(admitted)
            .is_err(),
        "retrying an admitted occurrence never allocates replacements again"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .companion_context_ids(&second_id, 0)
            .is_err()
    );
    assert!(
        fixture
            .snapshot
            .queue
            .companion_context_ids(&second_id, 2)
            .is_err()
    );
    assert_eq!(
        fixture.snapshot.drafts[0].1.text,
        "Newer independent current"
    );
}

#[test]
fn canonical_snapshots_reject_malformed_or_cross_owner_source_occurrence_tables() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let (mut fixture, first) = preserved_consumer_fixture(
        &mut journal,
        &["msg_original_first", "msg_original_second"],
        false,
    );
    let next = next_preserved_consumer(&fixture.snapshot, false);
    let next_id = next.id().clone();
    admit_occurrence(
        &mut fixture.snapshot,
        next,
        vec!["msg_rebound_first".into(), "msg_rebound_second".into()],
    );
    for mutation in 0..15 {
        let mut candidate = fixture.snapshot.queue.clone();
        let consumer = candidate
            .operations
            .iter_mut()
            .find(|operation| operation.id == next_id)
            .expect("second consumer");
        match mutation {
            0 => consumer.source_occurrences.as_mut().expect("table")[0].source_index = 0,
            1 => {
                consumer.source_occurrences.as_mut().expect("table")[0].take =
                    QueueOperationId::new("foreign_take").expect("ID")
            }
            2 => consumer.source_occurrences.as_mut().expect("table")[0].item_ordinal = 1,
            3 => consumer.source_occurrences.as_mut().expect("table")[0].source_sha256 = [4; 32],
            4 => consumer.source_occurrences.as_mut().expect("table")[0].index_sha256 = [5; 32],
            5 => consumer.source_occurrences.as_mut().expect("table")[0].payload_sha256 = [6; 32],
            6 => consumer.source_occurrences.as_mut().expect("table")[0].context_rebinding = None,
            7 => {
                consumer.source_occurrences.as_mut().expect("table")[0].context_rebinding =
                    Some(vec!["msg_original_first".into(), "msg_new".into()])
            }
            8 => {
                consumer.source_occurrences.as_mut().expect("table")[0].context_rebinding =
                    Some(vec!["msg_rebound_first".into()])
            }
            9 => {
                consumer.source_occurrences.as_mut().expect("table")[0].context_rebinding =
                    Some(vec!["msg_same".into(), "msg_same".into()])
            }
            10 => consumer.source_occurrences = None,
            11 => consumer.owner.runtime = "foreign-runtime".into(),
            12 => consumer.take_source_use = Some(QueueTakeSourceUse::Unused),
            13 => {
                candidate
                    .operations
                    .iter_mut()
                    .find(|operation| operation.id == fixture.id)
                    .expect("Take")
                    .take_source_use = None
            }
            _ => {
                candidate
                    .operations
                    .iter_mut()
                    .find(|operation| operation.id == fixture.id)
                    .expect("Take")
                    .take_source_use = Some(QueueTakeSourceUse::Unused)
            }
        }
        assert!(
            candidate
                .validate(
                    &fixture.snapshot.attachments,
                    &fixture.snapshot.preparations,
                    &[],
                    &mut HashSet::new()
                )
                .is_err(),
            "malformed occurrence metadata cannot weaken canonical source custody"
        );
    }
    let mut encoded =
        serde_json::to_value(&fixture.snapshot.queue).expect("exact table serialization");
    let consumer = encoded["operations"]
        .as_array_mut()
        .expect("operations")
        .iter_mut()
        .find(|operation| operation["id"] == next_id.as_str())
        .expect("second consumer");
    consumer["source_occurrences"][0]
        .as_object_mut()
        .expect("source occurrence")
        .remove("context_rebinding");
    assert!(
        serde_json::from_value::<QueueStorage>(encoded).is_err(),
        "missing rebinding is not an implicit original-ID grant"
    );
    assert_eq!(
        fixture
            .snapshot
            .queue
            .companion_context_ids(&first, 1)
            .expect("first-use IDs remain exact"),
        ["msg_original_first", "msg_original_second"]
    );
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("all corruption tests leave the actual owners unchanged");
}

#[test]
fn legacy_none_history_keeps_pending_original_retry_ids_and_refuses_new_source_claims() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let contexts = ["msg_legacy_original_first", "msg_legacy_original_second"];
    let (mut fixture, first) = preserved_consumer_fixture(&mut journal, &contexts, false);
    let mut encoded =
        serde_json::to_value(&fixture.snapshot.queue).expect("original pending owners");
    for operation in encoded["operations"].as_array_mut().expect("operations") {
        let operation = operation.as_object_mut().expect("operation");
        operation.remove("take_source_use");
        operation.remove("source_occurrences");
    }
    fixture.snapshot.queue =
        serde_json::from_value(encoded).expect("older v8 absent occurrence fields");
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("legacy pending snapshot remains readable");
    let next = next_preserved_consumer(&fixture.snapshot, false);
    let before = serde_json::to_vec(&fixture.snapshot.queue).expect("unknown use history");
    assert!(
        fixture
            .snapshot
            .queue
            .required_companion_rebinding_ids(&next)
            .is_err()
    );
    assert!(
        fixture
            .snapshot
            .queue
            .admit_composer_operation(
                next,
                vec!["msg_new_first".into(), "msg_new_second".into()],
                |_| Ok(())
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue)
            .expect("new refusal cannot rewrite legacy state"),
        before
    );
    let QueueAction::Enqueue {
        phase: QueueEnqueuePhase::Accepted { receipt, .. },
        ..
    } = fixture
        .snapshot
        .queue
        .operation(&first)
        .expect("legacy enqueue")
        .action()
    else {
        panic!("accepted original enqueue");
    };
    let mut observation = receipt.value().expect("original receipt");
    observation["state"] = serde_json::json!("uncertain");
    fixture
        .snapshot
        .queue
        .record_enqueue_accepted(&first, queue_json(observation), |_| Ok(()))
        .expect("original pending observation remains valid");
    fixture
        .snapshot
        .queue
        .prepare_enqueue_retry(
            &first,
            queue_json(serde_json::json!({"directory":"/project","attempt":1})),
            |_| Ok(()),
        )
        .expect("explicit original retry retains its request and IDs");
    assert_eq!(
        fixture
            .snapshot
            .queue
            .companion_context_ids(&first, 1)
            .expect("legacy retry's exact original context IDs"),
        contexts
    );
    assert!(
        fixture
            .snapshot
            .queue
            .operation(&first)
            .expect("legacy pending consumer")
            .source_occurrences()
            .is_none()
    );
    assert!(
        fixture
            .snapshot
            .queue
            .operation(&fixture.id)
            .expect("legacy Take")
            .take_source_use()
            .is_none()
    );
    save(&mut journal, &fixture.snapshot);
    drop(journal);
    let (_, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("reload original pending retry");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("retained original retry");
    };
    assert_eq!(
        restored
            .queue
            .companion_context_ids(&first, 1)
            .expect("original IDs after reload"),
        contexts
    );
    assert!(matches!(
        restored
            .queue
            .operation(&first)
            .expect("legacy retry")
            .action(),
        QueueAction::Enqueue {
            phase: QueueEnqueuePhase::RetryPrepared { .. },
            ..
        }
    ));
}

#[test]
fn legacy_unknown_context_free_sources_cannot_gain_new_claims_or_unmapped_shared_custody() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let mut fixture = acknowledged_edit(&mut journal);
    fixture
        .snapshot
        .queue
        .operations
        .iter_mut()
        .find(|operation| operation.id == fixture.id)
        .expect("original Take")
        .take_source_use = None;
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("legacy Take without known use history remains readable");
    let native = native_text();
    let first = local_send_operation(
        native.clone(),
        fixture.snapshot.queue.draft_companions(&native.owner),
    );
    assert!(
        fixture
            .snapshot
            .queue
            .required_companion_rebinding_ids(&first)
            .is_err(),
        "no active consumer cannot prove an older Take was unused"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .admit_composer_operation(first.clone(), Vec::new(), |_| Ok(()))
            .is_err(),
        "zero contexts still require proven occurrence custody"
    );
    // Reconstruct a previously pending legacy consumer, whose existing original
    // identities remain valid. New admission above cannot create this authority.
    fixture.snapshot.queue.operations.push(first);
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("one existing legacy pending consumer remains valid");
    fixture
        .snapshot
        .drafts
        .push((native.owner, Draft::new("Newer independent current")));
    let next = next_preserved_consumer(&fixture.snapshot, true);
    fixture.snapshot.queue.operations.push(next);
    assert!(
        JournalView::from(&fixture.snapshot).validate().is_err(),
        "canonical validation keeps legacy unproven sharing exclusive even without context IDs"
    );
}

#[test]
fn exact_consumer_preservation_survives_current_clear_without_borrowing_another_consumers_source() {
    let scratch = Scratch::new();
    let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
    let (mut fixture, first) = preserved_consumer_fixture(
        &mut journal,
        &["msg_original_first", "msg_original_second"],
        true,
    );
    let owner = source().owner;
    let references = fixture
        .snapshot
        .queue
        .operation(&first)
        .expect("published first consumer")
        .dependencies()
        .to_vec();
    let next = next_preserved_consumer(&fixture.snapshot, false);
    let next_id = next.id().clone();
    admit_occurrence(
        &mut fixture.snapshot,
        next,
        vec!["msg_rebound_first".into(), "msg_rebound_second".into()],
    );
    assert!(
        fixture
            .snapshot
            .queue
            .consume_composer_capture(
                &next_id,
                &owner,
                &references,
                None,
                ComposerConsumption::PreserveCurrent,
                |_| Ok(())
            )
            .is_err(),
        "an unpublished repeat cannot borrow the first consumer's published source"
    );
    fixture
        .snapshot
        .queue
        .clear_composer_document(&owner, |_| Ok(()))
        .expect("current-only clear leaves captured consumers independent");
    assert!(fixture.snapshot.queue.draft_companions(&owner).is_empty());
    let before = serde_json::to_vec(&fixture.snapshot.queue)
        .expect("captured ownership after current clear");
    fixture
        .snapshot
        .queue
        .consume_composer_capture(
            &first,
            &owner,
            &references,
            None,
            ComposerConsumption::PreserveCurrent,
            |candidate| {
                candidate.validate(
                    &fixture.snapshot.attachments,
                    &fixture.snapshot.preparations,
                    &[],
                    &mut HashSet::new(),
                )
            },
        )
        .expect("exact published capture remains owned without live current membership");
    assert_eq!(
        serde_json::to_vec(&fixture.snapshot.queue).expect("preserve is a semantic no-op"),
        before
    );
    assert!(
        fixture
            .snapshot
            .queue
            .consume_composer_capture(
                &first,
                &owner,
                &references,
                None,
                ComposerConsumption::Consume,
                |_| Ok(())
            )
            .is_err(),
        "destructive consumption still requires the live current source order"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .consume_composer_capture(
                &fixture.id,
                &owner,
                &references,
                None,
                ComposerConsumption::PreserveCurrent,
                |_| Ok(())
            )
            .is_err(),
        "the Take itself grants no outgoing consumer authority"
    );
    assert!(
        fixture
            .snapshot
            .queue
            .consume_composer_capture(
                &first,
                &owner,
                &[],
                None,
                ComposerConsumption::PreserveCurrent,
                |_| Ok(())
            )
            .is_err(),
        "exact consumer dependencies cannot be substituted"
    );
    let foreign = DraftKey::new(
        RuntimeId::from("foreign-runtime"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_queue")),
    )
    .expect("foreign draft owner");
    assert!(
        fixture
            .snapshot
            .queue
            .consume_composer_capture(
                &first,
                &foreign,
                &references,
                None,
                ComposerConsumption::PreserveCurrent,
                |_| Ok(())
            )
            .is_err()
    );
    assert!(
        fixture
            .snapshot
            .queue
            .references_native(&AttachmentId::from(fixture.local_blob.as_str()))
    );
    assert_eq!(
        fixture.snapshot.drafts[0].1.text,
        "Newer typing while the first consumer prepares"
    );
    JournalView::from(&fixture.snapshot)
        .validate()
        .expect("independent captured source roots remain valid");
}

#[test]
fn exact_consumer_consumption_refusal_restores_current_and_terminal_consumers_cannot_clear_it() {
    for prefix in [false, true] {
        let scratch = Scratch::new();
        let (mut journal, _) = AttachmentJournal::open(&scratch.journal()).expect("journal");
        let (mut fixture, consumer) = preserved_consumer_fixture(&mut journal, &[], prefix);
        let owner = source().owner;
        let references = fixture.snapshot.queue.draft_companions(&owner);
        let before = serde_json::to_vec(&fixture.snapshot.queue)
            .expect("current composer and all source owners");
        if prefix {
            assert!(
                fixture
                    .snapshot
                    .queue
                    .consume_composer_capture(
                        &consumer,
                        &owner,
                        &references,
                        None,
                        ComposerConsumption::Consume,
                        |candidate| {
                            assert!(candidate.draft_companions(&owner).is_empty());
                            Err(JournalError::Invalid("consumption save refused"))
                        }
                    )
                    .is_err()
            );
        } else {
            assert!(
                fixture
                    .snapshot
                    .queue
                    .consume_composer_capture(
                        &consumer,
                        &owner,
                        &references,
                        None,
                        ComposerConsumption::PreserveCurrent,
                        |_| Ok(())
                    )
                    .is_err(),
                "an accepted enqueue no longer has pre-dispatch consumption authority"
            );
        }
        assert_eq!(
            serde_json::to_vec(&fixture.snapshot.queue)
                .expect("refusal restores the exact current owner"),
            before
        );
    }
}

#[test]
fn confirmed_local_send_deletion_rolls_back_and_restarts_with_a_pristine_preparation_marker() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private deletion journal");
    let native = native_text();
    let runtime = native.owner.runtime().clone();
    let session = SessionId::from("ses_queue");
    let mut operation = local_send_operation(native, Vec::new());
    operation
        .begin_composer_preparation(1, 3)
        .expect("actual pristine preparation marker");
    let id = operation.id().clone();
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, operation);
    let before = serde_json::to_value(&snapshot.queue).expect("complete original ownership");
    snapshot
        .queue
        .delete_local_sends(
            &RuntimeId::from("another-runtime"),
            std::slice::from_ref(&session),
            |_| Ok(()),
        )
        .expect("another runtime is a semantic no-op");
    assert_eq!(
        serde_json::to_value(&snapshot.queue).expect("unchanged owner"),
        before
    );
    assert!(
        snapshot
            .queue
            .delete_local_sends(&runtime, std::slice::from_ref(&session), |_| Err(
                JournalError::Invalid("deletion save refused")
            ))
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(&snapshot.queue).expect("refusal restores all ownership"),
        before
    );
    let changed = snapshot
        .queue
        .delete_local_sends(&runtime, std::slice::from_ref(&session), |candidate| {
            candidate.validate(
                &snapshot.attachments,
                &snapshot.preparations,
                &[],
                &mut HashSet::new(),
            )
        })
        .expect("confirmed deletion permits an incomplete marker");
    assert_eq!(changed, std::slice::from_ref(&id));
    let mut expected = before;
    expected["operations"][0]["action"]["send"]["phase"] = serde_json::json!("deleted");
    assert_eq!(
        serde_json::to_value(&snapshot.queue).expect("only the terminal phase changed"),
        expected,
        "source seals, configuration, intent and occurrence metadata are immutable"
    );
    assert!(snapshot.queue.mark_send_unknown(&id, |_| Ok(())).is_err());
    assert!(
        snapshot
            .queue
            .record_send_admitted(&id, &MessageId::from("msg_local_original"), |_| Ok(()))
            .is_err(),
        "even an exact late admission callback cannot reopen a deleted owner"
    );
    let marker = snapshot
        .queue
        .operation(&id)
        .expect("terminal owner")
        .composer_preparation()
        .expect("retained marker")
        .clone();
    assert!(
        snapshot
            .queue
            .extend_composer_preparation(
                &id,
                marker,
                &snapshot.attachments,
                &mut snapshot.preparations,
                |_, _| Ok(()),
                |_, _| Ok(())
            )
            .is_err(),
        "Deleted grants no new preparation authority"
    );
    snapshot
        .queue
        .begin_local_send_retirement(&id, Vec::new(), |_| Ok(()))
        .expect("local cleanup needs no frame or inputs");
    save(&mut journal, &snapshot);
    drop(journal);

    let (mut journal, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("restart with deleted preparation");
    let LoadedJournal::Present(mut restored) = loaded else {
        panic!("deleted source and cleanup remain durable");
    };
    assert!(restored.queue.local_send_deleted(&id));
    assert!(
        restored
            .queue
            .local_send_retirement(&id)
            .expect("cleanup cause survives")
            .deleted
    );
    assert!(restored.queue.byte_authority().acknowledgements.is_empty());
    let mut state = crate::queue_local_send::LocalSendState::default();
    state.restore(&restored.queue);
    assert!(
        !state.any_busy(),
        "hydration starts no HTTP, repair or cleanup"
    );
    assert_ne!(
        state.check(&id),
        crate::queue_local_send::LocalSendCheck::Admitted
    );
    let before =
        serde_json::to_vec(&restored.queue).expect("deleted cleanup before retirement refusal");
    assert!(
        restored
            .queue
            .retire_operation(&id, |_| Err(JournalError::Invalid(
                "retirement save refused"
            )))
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&restored.queue).expect("all terminal owners remain"),
        before
    );
    restored
        .queue
        .retire_operation(&id, |_| Ok(()))
        .expect("terminal local-only retirement");
    save(&mut journal, &restored);
    restored
        .queue
        .finish_local_send_retirement(&id, |_| Ok(()))
        .expect("no server admission is needed to finish local cleanup");
    save(&mut journal, &restored);
}

#[test]
fn deleted_partial_preparation_keeps_catalog_and_exact_holds_but_refuses_late_publication() {
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("private partial deletion journal");
    let mut native = native_text();
    native.draft = Draft::new("@doc.txt @later.txt");
    let input = preparation_input(&native);
    let runtime = native.owner.runtime().clone();
    let mut snapshot = JournalSnapshot::default();
    admit(&mut snapshot, local_send_operation(native, Vec::new()));
    let id = snapshot.queue.operations()[0].id().clone();
    let proposal = preparation_plan(
        snapshot.queue.operation(&id).expect("original owner"),
        &input,
        vec![derived_file(710)],
        Vec::new(),
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            proposal,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("real explicit mention plan");
    save(&mut journal, &snapshot);
    let receipt = journal
        .put(&snapshot.preparations[0], b"derived")
        .expect("actual first output bytes");
    snapshot
        .queue
        .publish_composer_derived_file(
            &id,
            1,
            0,
            &receipt,
            &mut snapshot.attachments,
            &mut snapshot.preparations,
            |_, _, _| Ok(()),
        )
        .expect("first output catalog custody");
    let mut extended = snapshot
        .queue
        .operation(&id)
        .expect("original owner")
        .composer_preparation()
        .expect("partial plan")
        .clone();
    let mut later = derived_file(711);
    later.mention = QueuePreparationMention {
        start: 9,
        end: 19,
        text: "@later.txt".into(),
    };
    later.server_path = "/project/later.txt".into();
    later.name = "later.txt".into();
    later.document_group = Some("later-document".into());
    extended.files.push(later);
    snapshot
        .queue
        .extend_composer_preparation(
            &id,
            extended,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_preparation_input(&input, plan),
            |_, _| Ok(()),
        )
        .expect("second exact pre-put obligation");
    save(&mut journal, &snapshot);
    let hold = snapshot.preparations[0].clone();
    let late = journal
        .put(&hold, b"derived")
        .expect("second put completed before its publication");
    snapshot
        .queue
        .delete_local_sends(&runtime, &[SessionId::from("ses_queue")], |candidate| {
            candidate.validate(
                &snapshot.attachments,
                &snapshot.preparations,
                &[],
                &mut HashSet::new(),
            )
        })
        .expect("confirmed deletion preserves partial preparation");
    let before = serde_json::to_vec(&snapshot.queue).expect("terminal partial metadata");
    assert!(
        snapshot
            .queue
            .publish_composer_derived_file(
                &id,
                1,
                1,
                &late,
                &mut snapshot.attachments,
                &mut snapshot.preparations,
                |_, _, _| Ok(())
            )
            .is_err(),
        "a late worker receipt grants no publication authority"
    );
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("immutable terminal preparation"),
        before
    );
    assert_eq!(snapshot.preparations, [hold]);
    assert_eq!(snapshot.attachments.len(), 1);
    let files = snapshot
        .queue
        .composer_file_descriptors(&id)
        .expect("cataloged and proposed exact cleanup obligations");
    assert_eq!(files.len(), 2);
    snapshot
        .queue
        .begin_local_send_retirement(&id, files, |_| Ok(()))
        .expect("cleanup can retain every partial output without reading inputs");
    save(&mut journal, &snapshot);
    drop(journal);
    let (_, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("restart with deleted partial files");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("partial deletion remains recoverable");
    };
    assert!(
        restored
            .queue
            .local_send_retirement(&id)
            .expect("retained local cleanup")
            .deleted
    );
    assert_eq!(restored.preparations.len(), 1);
    assert!(
        restored
            .queue
            .operation(&id)
            .expect("retained terminal owner")
            .composer_preparation()
            .expect("partial preparation")
            .inputs_object()
            .is_none()
    );
    JournalView::from(&*restored)
        .validate()
        .expect("Deleted keeps roots without inventing complete inputs");
}
