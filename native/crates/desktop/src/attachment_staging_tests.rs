//! Pure ownership and fixed-target checks for the staging worker.

use openchamber_native_core::draft::{Draft, DraftTarget};
use openchamber_native_core::ids::DraftId;

use crate::attachment_spool::{BlobDescriptor, BlobId};

use super::*;

fn owner(runtime: &str, directory: &str, draft: &str) -> DraftKey {
    DraftKey::new(
        RuntimeId::from(runtime),
        directory,
        DraftTarget::NewSession(DraftId::from(draft)),
    )
    .expect("fixture owner has an absolute directory")
}

fn request(value: u64) -> StagingRequest {
    StagingRequest {
        owner: owner("runtime", "/project", "draft"),
        id: AttachmentId::from(format!("{value:032x}")),
    }
}

fn fixture() -> (DraftStore, AttachmentCatalog, StagingJob) {
    let request = request(1);
    let file = DraftFile {
        id: request.id.clone(),
        name: Some("captured %.txt".to_owned()),
        media_type: Some("text/plain".to_owned()),
        byte_len: Some(3),
        source_document_id: None,
        content: DraftFileContent::Prepared,
    };
    let prepared = PreparedAttachment {
        blob: BlobDescriptor::new(
            BlobId::parse(request.id.as_str()).expect("fixture blob label"),
            3,
            [7; 32],
        ),
        staging: None,
    };
    let mut drafts = DraftStore::new();
    let mut draft = Draft::new("independent typing");
    draft.content.files.push(file.clone());
    drafts.save(request.owner.clone(), draft);
    let catalog = [(request.id.clone(), prepared.clone())]
        .into_iter()
        .collect();
    let job = StagingJob {
        generation: 1,
        canceled: false,
        request,
        file,
        prepared,
    };
    (drafts, catalog, job)
}

fn target(phase: StagingPhase) -> StagingDescriptor {
    let server_path = "/tmp/opencode/openchamber/0123456789abcdef/captured %.txt".to_owned();
    StagingDescriptor {
        runtime: RuntimeId::from("runtime"),
        directory: "/project".to_owned(),
        temporary_directory: "/tmp/opencode".to_owned(),
        uri: attachment_catalog::file_uri(&server_path),
        server_path,
        filename: "captured %.txt".to_owned(),
        phase,
    }
}

#[test]
fn duplicate_activations_are_noops_and_capacity_includes_the_active_file() {
    let mut staging = AttachmentStaging::default();
    let (_, _, active) = fixture();
    staging.active = Some(active.clone());
    for _ in 0..3 {
        assert!(staging.enqueue(active.request.clone()));
    }
    assert!(
        staging.pending.is_empty(),
        "an active file cannot be retried implicitly"
    );
    for value in 2..=MAX_ENTRIES as u64 {
        assert!(staging.enqueue(request(value)));
    }
    assert_eq!(staging.pending.len(), MAX_ENTRIES - 1);
    assert!(!staging.enqueue(request(MAX_ENTRIES as u64 + 1)));
    assert!(
        staging.enqueue(request(2)),
        "a duplicate succeeds at capacity"
    );
    assert_eq!(staging.pending.len(), MAX_ENTRIES - 1);
}

#[test]
fn releasing_a_failed_job_preserves_other_files_and_owner_order() {
    let mut staging = AttachmentStaging::default();
    let (_, _, active) = fixture();
    staging.active = Some(active);
    let first = request(2);
    let mut other_owner = request(3);
    other_owner.owner = owner("other-runtime", "/other", "other-draft");
    assert!(staging.enqueue(first.clone()));
    assert!(staging.enqueue(other_owner.clone()));
    staging.active = None;
    assert!(staging.pending.pop_front() == Some(first));
    assert!(staging.pending.pop_front() == Some(other_owner));
    assert!(!staging.is_busy());
}

#[test]
fn active_ready_wait_keeps_its_runtime_and_withholds_ready_consumption() {
    let mut staging = AttachmentStaging::default();
    let (_, _, mut job) = fixture();
    job.prepared.staging = Some(target(StagingPhase::Ready));
    staging.active = Some(job.clone());
    assert!(staging.is_busy(), "Ready still needs its durable save");
    assert!(staging.contains(&job.request.owner, &job.request.id));
    assert!(staging.stages_id(&job.request.id));
    let other_owner = owner("runtime", "/project", "another-draft");
    assert!(!staging.contains(&other_owner, &job.request.id));
    assert!(
        staging.stages_id(&job.request.id),
        "every owner waits for the catalog's Ready save"
    );
    assert!(staging.holds_runtime(job.request.owner.runtime()));
    assert!(!staging.holds_runtime(&RuntimeId::from("other-runtime")));
    staging.active = None;
    assert!(!staging.contains(&job.request.owner, &job.request.id));
    assert!(!staging.stages_id(&job.request.id));
    assert!(!staging.holds_runtime(job.request.owner.runtime()));
}

#[test]
fn draft_removal_catalog_removal_and_same_id_replacement_retire_capture_authority() {
    let (mut drafts, mut catalog, job) = fixture();
    assert!(job.still_owned(&drafts, &catalog));
    let saved = drafts.take(&job.request.owner).expect("fixture draft");
    assert!(
        !job.still_owned(&drafts, &catalog),
        "discard never grants resurrection authority"
    );
    drafts.save(job.request.owner.clone(), saved.clone());
    let descriptor = catalog
        .remove(&job.request.id)
        .expect("fixture catalog entry");
    assert!(!job.still_owned(&drafts, &catalog));
    catalog.insert(job.request.id.clone(), descriptor);
    let mut edited = saved;
    edited.content.files[0].media_type = Some("application/pdf".to_owned());
    drafts.save(job.request.owner.clone(), edited);
    assert!(
        !job.still_owned(&drafts, &catalog),
        "all immutable DraftFile fields must match"
    );
}

#[test]
fn unrelated_typing_preserves_capture_but_a_digest_change_cannot_supply_new_bytes() {
    let (mut drafts, mut catalog, job) = fixture();
    let mut draft = drafts.take(&job.request.owner).expect("fixture draft");
    draft.text = "newer typing".to_owned();
    draft.selection = 0..0;
    drafts.save(job.request.owner.clone(), draft);
    assert!(job.still_owned(&drafts, &catalog));
    catalog
        .get_mut(&job.request.id)
        .expect("fixture entry")
        .blob = BlobDescriptor::new(*job.prepared.blob.id(), 3, [8; 32]);
    assert!(
        !job.still_owned(&drafts, &catalog),
        "length and ID alone grant no read authority"
    );
}

#[test]
fn retained_target_accepts_only_phase_changes_and_keeps_the_original_uri() {
    let (_, _, mut job) = fixture();
    for phase in [
        StagingPhase::Planned,
        StagingPhase::Writing,
        StagingPhase::Uncertain,
    ] {
        job.prepared.staging = Some(target(phase));
        for next in [
            StagingPhase::Writing,
            StagingPhase::Uncertain,
            StagingPhase::Ready,
        ] {
            assert!(job.accepts_target(&target(next)));
        }
        let mut replacement = target(StagingPhase::Writing);
        replacement.server_path = replacement
            .server_path
            .replace("0123456789abcdef", "fedcba9876543210");
        replacement.uri = attachment_catalog::file_uri(&replacement.server_path);
        assert!(
            !job.accepts_target(&replacement),
            "explicit retry cannot allocate a new target"
        );
        let mut changed_root = target(StagingPhase::Writing);
        changed_root.temporary_directory = "/tmp/replaced".to_owned();
        assert!(!job.accepts_target(&changed_root));
    }
}

#[test]
fn owner_and_catalog_association_are_required_even_before_a_target_exists() {
    let (drafts, mut catalog, job) = fixture();
    let mut other = job.request.clone();
    other.owner = owner("runtime", "/project", "different-draft");
    assert!(owned_prepared(&drafts, &catalog, &other.owner, &other.id).is_none());
    catalog
        .get_mut(&job.request.id)
        .expect("fixture entry")
        .staging = Some(target(StagingPhase::Uncertain));
    assert!(owned_prepared(&drafts, &catalog, &job.request.owner, &job.request.id).is_some());
    let retained = catalog.get_mut(&job.request.id).expect("fixture entry");
    retained.staging.as_mut().expect("fixture target").runtime = RuntimeId::from("other-runtime");
    assert!(owned_prepared(&drafts, &catalog, &job.request.owner, &job.request.id).is_none());
}

#[test]
fn removal_invalidation_dominates_identical_readdition_without_clearing_other_owners() {
    let (drafts, catalog, job) = fixture();
    let mut staging = AttachmentStaging {
        active: Some(job.clone()),
        ..AttachmentStaging::default()
    };
    assert!(staging.enqueue(request(2)));
    let mut other_owner = request(3);
    other_owner.owner = owner("other-runtime", "/other", "other-draft");
    assert!(staging.enqueue(other_owner.clone()));
    staging.invalidate(&job.request.owner, Some(&job.request.id));
    assert!(
        job.still_owned(&drafts, &catalog),
        "the same value may have been re-added"
    );
    assert!(
        staging.active.as_ref() != Some(&job),
        "the old completion remains retired"
    );
    assert!(staging.holds_runtime(job.request.owner.runtime()));
    assert_eq!(staging.pending.len(), 2, "unrelated queued work survives");
    staging.invalidate(&job.request.owner, None);
    assert_eq!(
        staging.pending.len(),
        1,
        "whole-draft discard cancels its own metadata"
    );
    assert!(staging.pending.front() == Some(&other_owner));
}

#[test]
fn restored_windows_root_spelling_can_be_validated_without_changing_its_fixed_target() {
    let (_, _, mut job) = fixture();
    job.request.owner = owner("runtime", "C:/project", "draft");
    let mut retained = target(StagingPhase::Uncertain);
    retained.directory = "c:\\project".to_owned();
    retained.temporary_directory = "c:\\Temp".to_owned();
    retained.server_path = "c:\\Temp\\openchamber\\0123456789abcdef\\captured %.txt".to_owned();
    retained.uri = attachment_catalog::file_uri(&retained.server_path);
    job.prepared.staging = Some(retained.clone());
    let mut planned = retained;
    planned.directory = "C:/project".to_owned();
    planned.temporary_directory = "C:/Temp".to_owned();
    planned.phase = StagingPhase::Writing;
    assert!(job.accepts_target(&planned));
    planned.server_path = planned.server_path.replace('\\', "/");
    assert!(
        !job.accepts_target(&planned),
        "the saved server-path spelling remains exact"
    );
}
