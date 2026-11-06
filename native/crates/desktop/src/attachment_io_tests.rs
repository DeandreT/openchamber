//! Exact composer-derived authority under captured and confirmed-deleted owners.

use super::*;

use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;

use crate::draft_companion::CompanionCapture;
use crate::queue_capture::{QueueNativeSource, QueueOperationId, sha256};
use crate::queue_storage::{
    QueueComposerPreparation, QueueConfiguration, QueueDerivedFile, QueueOperation, QueueOwner,
    QueuePreparationMention, QueuePreparationSource, QueuePreparationSourceId,
};

fn proposed_composer_file(kind: usize) -> (JournalSnapshot, PreparationHold, BlobDescriptor) {
    let owner = DraftKey::new(
        RuntimeId::from("runtime"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_preparation")),
    )
    .expect("exact owner");
    let native = QueueNativeSource {
        owner: owner.clone(),
        draft: Draft::new("@doc.txt"),
    };
    let queue_owner = QueueOwner::new(
        owner.runtime(),
        owner.directory(),
        &SessionId::from("ses_preparation"),
        &owner,
    )
    .expect("queue owner");
    let config = QueueConfiguration::capture(&QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "model".into(),
        agent: None,
        variant: None,
    });
    let id = QueueOperationId::new("composer_preparation").expect("operation ID");
    let operation = match kind {
        0 => QueueOperation::local_send(
            id.clone(),
            queue_owner,
            config,
            MessageId::from("msg_preparation"),
            CompanionCapture {
                native,
                references: Vec::new(),
            },
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("Send capture"),
        1 => QueueOperation::enqueue_with_edit(
            id.clone(),
            queue_owner,
            config,
            CompanionCapture {
                native,
                references: Vec::new(),
            },
            Vec::new(),
            Vec::new(),
            None,
        )
        .expect("Enqueue capture"),
        _ => {
            use crate::queue_storage::{NativePrefixIntent, NativePrefixKind};

            let intent = NativePrefixIntent {
                kind: NativePrefixKind::Capture,
                directory: "/project".into(),
                id: "msg_preparation".into(),
                item_ids: vec!["queued-original".into()],
                delivery: None,
                companions: Vec::new(),
                native_context_ids: Vec::new(),
                files: Vec::new(),
                edit: None,
            };
            QueueOperation::prefix(
                id.clone(),
                queue_owner,
                config,
                MessageId::from("msg_preparation"),
                native,
                QueueJson::new(serde_json::to_vec(&intent).expect("native intent"))
                    .expect("bounded intent"),
            )
            .expect("Prefix capture")
        }
    };
    let seal = *operation.source_sha256().expect("original source seal");
    let local = "00000000000000000000000000000901";
    let file = QueueDerivedFile {
        source: QueuePreparationSourceId::Source(0),
        mention: QueuePreparationMention {
            start: 0,
            end: 8,
            text: "@doc.txt".into(),
        },
        server_path: "/project/doc.txt".into(),
        raw_source_byte_len: 7,
        raw_source_sha256: sha256(b"derived"),
        output_ordinal: 0,
        local_attachment_id: local.into(),
        blob: local.into(),
        preparation_generation: 3,
        byte_len: 7,
        sha256: sha256(b"derived"),
        name: "doc.txt".into(),
        mime_type: "text/plain".into(),
        document_group: None,
        state: QueueDerivedFileState::Proposed,
    };
    let descriptor = file.descriptor().expect("exact expected output bytes");
    let hold = file.hold(&owner).expect("exact output hold");
    let plan = QueueComposerPreparation::new(
        &owner,
        seal,
        3,
        4,
        vec![QueuePreparationSource {
            identity: QueuePreparationSourceId::Source(0),
            byte_len: 8,
            sha256: sha256(b"@doc.txt"),
        }],
        vec![file],
        Vec::new(),
    );
    let mut snapshot = JournalSnapshot::default();
    assert!(
        snapshot
            .queue
            .admit_operation(operation, |_| Ok(()))
            .is_ok()
    );
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            plan,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| {
                if plan.sources[0].byte_len != 8 || plan.sources[0].sha256 != sha256(b"@doc.txt") {
                    return Err(JournalError::Invalid("captured input differs"));
                }
                Ok(())
            },
            |_, _| Ok(()),
        )
        .expect("exact operation-owned proposal");
    (snapshot, hold, descriptor)
}

#[test]
fn capturing_send_enqueue_and_prefix_proposals_grant_only_the_exact_resume_descriptor() {
    for kind in 0..3 {
        let (snapshot, hold, descriptor) = proposed_composer_file(kind);
        let authority = AttachmentAuthority::capture(&snapshot);
        assert_eq!(
            authority.check_attachment_resume(&hold, &descriptor),
            Ok(()),
            "exact admitted proposal can resume"
        );
        let changed =
            BlobDescriptor::new(*descriptor.id(), descriptor.byte_len(), sha256(b"changed"));
        assert_eq!(
            authority.check_attachment_resume(&hold, &changed),
            Err(AttachmentFailure::Unauthorized)
        );
        let changed = BlobDescriptor::new(
            *descriptor.id(),
            descriptor.byte_len() + 1,
            *descriptor.sha256(),
        );
        assert_eq!(
            authority.check_attachment_resume(&hold, &changed),
            Err(AttachmentFailure::Unauthorized)
        );
        let mut changed = hold.clone();
        changed.generation += 1;
        assert_eq!(
            authority.check_attachment_resume(&changed, &descriptor),
            Err(AttachmentFailure::Unauthorized)
        );
        changed = hold.clone();
        changed.owner = DraftKey::new(
            RuntimeId::from("another-runtime"),
            "/project",
            DraftTarget::Session(SessionId::from("ses_preparation")),
        )
        .expect("foreign hold owner");
        assert_eq!(
            authority.check_attachment_resume(&changed, &descriptor),
            Err(AttachmentFailure::Unauthorized)
        );
        changed = hold.clone();
        changed.id = AttachmentId::from("unrelated-label");
        assert_eq!(
            authority.check_attachment_resume(&changed, &descriptor),
            Err(AttachmentFailure::Unauthorized)
        );
    }
}

#[test]
fn changed_snapshot_hold_owner_generation_and_removed_hold_grant_no_derived_repair() {
    for mutation in 0..3 {
        let (mut snapshot, hold, descriptor) = proposed_composer_file(0);
        match mutation {
            0 => snapshot.preparations[0].generation += 1,
            1 => {
                snapshot.preparations[0].owner = DraftKey::new(
                    RuntimeId::from("runtime"),
                    "/different-project",
                    DraftTarget::Session(SessionId::from("ses_preparation")),
                )
                .expect("different exact owner")
            }
            _ => snapshot.preparations.clear(),
        }
        let authority = AttachmentAuthority::capture(&snapshot);
        assert_eq!(
            authority.check_attachment_resume(&hold, &descriptor),
            Err(AttachmentFailure::Unauthorized)
        );
        if let Some(current) = snapshot.preparations.first() {
            assert_eq!(
                authority.check_attachment_resume(current, &descriptor),
                Err(AttachmentFailure::Unauthorized),
                "a changed hold cannot acquire repair merely by appearing in preparations"
            );
        }
    }
}

#[test]
fn cataloged_sealed_and_non_capturing_preparations_grant_no_repair_even_with_a_stale_hold() {
    for mutation in 0..4 {
        let (mut snapshot, hold, descriptor) = proposed_composer_file(0);
        let mut stored = serde_json::to_value(&snapshot.queue).expect("captured queue metadata");
        match mutation {
            0 => {
                stored["operations"][0]["preparation"]["files"][0]["state"] =
                    serde_json::json!("cataloged")
            }
            1 => {
                stored["operations"][0]["preparation"]["inputs_object"] =
                    serde_json::json!("sealed-inputs")
            }
            2 => {
                stored["operations"][0]["action"]["send"]["phase"] =
                    serde_json::json!({"prepared":{"frame":"frozen-frame"}})
            }
            _ => {
                snapshot.attachments.insert(
                    hold.id.clone(),
                    crate::attachment_catalog::PreparedAttachment {
                        blob: descriptor,
                        staging: None,
                    },
                );
            }
        }
        // Exercise the authority filters directly. A canonical save also rejects
        // these stale combinations before they can become worker authority.
        snapshot.queue = serde_json::from_value(stored).expect("typed altered metadata");
        let authority = AttachmentAuthority::capture(&snapshot);
        assert_eq!(
            authority.check_attachment_resume(&hold, &descriptor),
            Err(AttachmentFailure::Unauthorized)
        );
    }
}

#[test]
fn ordinary_preparation_keeps_put_authority_without_receiving_derived_repair_authority() {
    let (mut snapshot, hold, descriptor) = proposed_composer_file(0);
    snapshot.queue = crate::queue_storage::QueueStorage::default();
    let authority = AttachmentAuthority::capture(&snapshot);
    assert_eq!(
        authority.check_put(&hold),
        Ok(()),
        "ordinary preparation still permits its original put"
    );
    assert_eq!(
        authority.check_attachment_resume(&hold, &descriptor),
        Err(AttachmentFailure::Unauthorized)
    );
}

#[test]
fn committed_deleted_send_revokes_worker_writes_and_verification_while_retaining_byte_roots() {
    use crate::mutation_journal::JournalView;
    use crate::queue_capture::{QueueCaptureBytes, QueueObjectId, QueueObjectKind};
    use crate::queue_storage::QueueObject;

    let fixture = crate::recovery_tests::Fixture::new();
    let (mut journal, _) =
        AttachmentJournal::open(&fixture.journal()).expect("private serialized file owner");
    let (mut snapshot, hold, descriptor) = proposed_composer_file(0);
    let operation = snapshot.queue.operations()[0].id().clone();
    let source = serde_json::to_vec(
        &snapshot.queue.operations()[0]
            .local_send_capture()
            .expect("actual original capture"),
    )
    .expect("original source bytes");
    let mut captures = Vec::new();
    let mut holds = Vec::new();
    for (index, name) in ["deleted-source-unwritten", "deleted-source-ready"]
        .into_iter()
        .enumerate()
    {
        let blob = BlobId::parse(&format!("{:032x}", 0x910 + index)).expect("unique local chunk");
        let capture =
            QueueCaptureBytes::new(QueueObjectKind::NativeCompanion, source.clone(), &[blob])
                .expect("exact sealed original");
        let object_id = QueueObjectId::new(name).expect("unique original object");
        let object = QueueObject::planned(
            object_id.clone(),
            operation.clone(),
            QueueObjectKind::NativeCompanion,
            1,
            capture.reference().clone(),
        )
        .expect("source write intent");
        assert!(snapshot.queue.admit_object(object, |_| Ok(())).is_ok());
        holds.push(
            snapshot
                .queue
                .object(&object_id)
                .expect("planned object")
                .hold(0)
                .expect("exact chunk"),
        );
        captures.push(capture);
    }
    let ordinary_blob =
        BlobId::parse("00000000000000000000000000000920").expect("independent preparation label");
    let ordinary = PreparationHold {
        owner: DraftKey::new(
            hold.owner.runtime().clone(),
            "/project",
            DraftTarget::Session(SessionId::from("ses_independent")),
        )
        .expect("same runtime, independent session"),
        id: AttachmentId::from(ordinary_blob.as_str()),
        generation: hold.generation,
        blob: ordinary_blob,
    };
    snapshot.preparations.push(ordinary.clone());
    assert!(
        matches!(journal.save(&snapshot), Ok(JournalCommit::Durable)),
        "canonical source and proposal snapshot"
    );
    let original = journal
        .put(&hold, b"derived")
        .expect("exact live generic put");
    assert_eq!(
        original.descriptor, descriptor,
        "the fixture binds actual prepared bytes"
    );
    let receipt = journal
        .put_queue(&holds[1], captures[1].bytes())
        .expect("live source chunk put");
    snapshot
        .queue
        .record_chunk(&holds[1], &receipt)
        .expect("actual worker receipt");
    assert!(matches!(
        journal.save(&snapshot),
        Ok(JournalCommit::Durable)
    ));
    let verification = snapshot
        .queue
        .object(&holds[1].object)
        .expect("stored original")
        .verification();
    assert!(
        journal.verify_queue(&verification).is_ok(),
        "live stored source grants whole-stream verification"
    );
    let before = AttachmentAuthority::capture(&snapshot);
    assert_eq!(before.check_put(&hold), Ok(()));
    assert_eq!(before.check_attachment_resume(&hold, &descriptor), Ok(()));
    assert_eq!(before.check_queue_put(&holds[0]), Ok(()));
    assert_eq!(
        before.check_queue_resume(&holds[0]),
        Ok(()),
        "unwritten original grants an exact explicit repair"
    );
    let retained_holds = snapshot.queue.holds();

    let mut queue = std::mem::take(&mut snapshot.queue);
    let deleted = queue
        .delete_local_sends(
            hold.owner.runtime(),
            &[SessionId::from("ses_preparation")],
            |candidate| {
                let mut view = JournalView::from(&snapshot);
                view.queue = candidate;
                view.validate()
            },
        )
        .expect("real confirmed-deletion transition");
    snapshot.queue = queue;
    assert_eq!(deleted, vec![operation.clone()]);
    assert!(snapshot.queue.local_send_deleted(&operation));
    assert_eq!(
        snapshot.queue.holds(),
        retained_holds,
        "revocation must preserve every retained object hold"
    );
    assert!(
        snapshot.preparations.contains(&hold),
        "the Proposed output retains exact byte custody"
    );
    assert_eq!(
        journal
            .put(&hold, b"derived")
            .expect("a pending in-memory transition cannot replace committed worker authority"),
        original
    );
    assert!(
        matches!(journal.save(&snapshot), Ok(JournalCommit::Durable)),
        "deletion authority commits before revocation"
    );
    drop(journal);
    let (mut journal, loaded) =
        AttachmentJournal::open(&fixture.journal()).expect("deleted authority survives restart");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("committed deletion remains present");
    };
    assert!(restored.queue.local_send_deleted(&operation));
    assert_eq!(
        journal.put(&hold, b"derived"),
        Err(AttachmentFailure::Unauthorized),
        "a retained Deleted hold grants no generic put"
    );
    assert_eq!(
        journal.put_downloaded_attachment(&hold, &descriptor, b"derived"),
        Err(AttachmentFailure::Unauthorized)
    );
    assert_eq!(
        journal.resume_attachment(&hold, &descriptor, b"derived"),
        Err(AttachmentFailure::Unauthorized)
    );
    assert_eq!(
        journal.put_queue(&holds[0], captures[0].bytes()),
        Err(AttachmentFailure::Unauthorized)
    );
    assert_eq!(
        journal.resume_queue(&holds[0], captures[0].bytes()),
        Err(AttachmentFailure::Unauthorized)
    );
    assert_eq!(
        journal.verify_queue(&verification),
        Err(AttachmentFailure::Unauthorized)
    );
    assert_eq!(
        journal
            .read_queue(&receipt.descriptor)
            .expect("stored original remains readable")
            .as_slice(),
        captures[1].bytes()
    );
    assert_eq!(
        journal.release(&hold.blob),
        Err(AttachmentFailure::Unauthorized)
    );
    assert_eq!(
        journal.release_queue(receipt.descriptor.id()),
        Err(AttachmentFailure::Unauthorized)
    );
    let mut authority = AttachmentAuthority::capture(&restored);
    authority.retired_blobs.push(hold.blob);
    authority.queue.retired.push(*receipt.descriptor.id());
    assert_eq!(
        authority.check_release(&hold.blob),
        Err(AttachmentFailure::Unauthorized),
        "retained proposal excludes a premature release"
    );
    assert_eq!(
        authority.check_queue_release(receipt.descriptor.id()),
        Err(AttachmentFailure::Unauthorized),
        "retained original excludes a premature release"
    );
    let independent = journal
        .put(&ordinary, b"independent")
        .expect("deletion preserves another session's generic preparation");
    assert_eq!(independent.descriptor.id(), &ordinary.blob);
}
