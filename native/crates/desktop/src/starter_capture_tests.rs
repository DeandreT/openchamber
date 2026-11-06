use super::*;
use openchamber_native_core::draft::{DraftStore, DraftTarget};
use openchamber_native_core::draft_content::{
    AgentMention, DraftContext, DraftFile, DraftFileContent, Mention, SkillRef,
};
use openchamber_native_core::ids::{AttachmentId, ContextItemId, DraftId, RuntimeId};
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_transport::prompt_parts::{Metadata, PromptFile};

fn owner(runtime: &str, directory: &str) -> DraftKey {
    DraftKey::new(
        RuntimeId::from(runtime),
        directory,
        DraftTarget::NewSession(DraftId::from("draft")),
    )
    .expect("absolute test directory")
}

fn source(id: &str, text: &str) -> Draft {
    let mut draft = Draft::new(text);
    draft.selection = 0..0;
    draft.content.files.push(DraftFile {
        id: AttachmentId::from(id),
        name: Some(format!("{id}.txt")),
        media_type: Some("text/plain".to_owned()),
        byte_len: Some(3),
        source_document_id: None,
        content: DraftFileContent::Reference {
            uri: format!("file:///source/{id}.txt"),
        },
    });
    draft.content.context.push(DraftContext {
        id: ContextItemId::from(id),
        text: format!("{id} selection"),
        description: Some("editor selection".to_owned()),
        metadata_json: Some("{\"source\":\"editor\"}".to_owned()),
        instructions: Some(format!("{id} instructions")),
    });
    draft.content.skills.push(SkillRef { id: id.to_owned() });
    draft.content.metadata_json = Some(format!("{{\"draft\":\"{id}\"}}"));
    draft
}

fn capture(id: &str, owner: DraftKey, draft: Draft) -> StarterCapture {
    // Production converts these through capture_draft_parts before the read.
    // Tests exercise capture ownership with concrete already-converted parts.
    StarterCapture {
        id: MessageId::from(id),
        owner,
        project: "project".to_owned(),
        preset_text: format!("/explore {}", draft.text.trim()),
        parts: PromptParts {
            files: vec![PromptFile {
                uri: "file:///retained/original.txt".to_owned(),
                name: Some("original.txt".to_owned()),
            }],
            agents: draft.content.agents.clone(),
            skills: draft.content.skills.clone(),
            metadata: Some(Metadata(serde_json::Map::from_iter([(
                "draft".to_owned(),
                serde_json::Value::String("original".to_owned()),
            )]))),
            context: vec![
                SyntheticContext {
                    id: MessageId::from(format!("{id}-selection")),
                    text: "original selection".to_owned(),
                    description: Some("editor selection".to_owned()),
                    metadata: None,
                },
                SyntheticContext {
                    id: MessageId::from(format!("{id}-instructions")),
                    text: "original instructions".to_owned(),
                    description: None,
                    metadata: None,
                },
            ],
        },
        draft,
        selection: Selection {
            agent_name: Some("plan".to_owned()),
            model: Some(ModelChoice {
                provider_id: "provider".to_owned(),
                model_id: "original".to_owned(),
                variant: Some("high".to_owned()),
            }),
        },
        permission_mode: Some(PermissionMode::Safety),
    }
}

#[test]
fn capture_and_handoff_leave_newer_text_files_context_and_choices_independent() {
    let owner = owner("a", "/p");
    let original = source("original", "same text");
    let capture = capture("activation", owner.clone(), original.clone());
    let mut drafts = DraftStore::new();
    drafts.save(owner.clone(), original);
    let mut captures = StarterCaptures::default();
    assert!(captures.begin(capture.clone(), &mut drafts));
    assert!(
        drafts.get(&owner).is_none(),
        "the capture owns all original content"
    );
    assert!(!captures.can_begin(&owner));

    let newer = source("newer", "same text");
    drafts.save(owner.clone(), newer.clone());
    let newer_selection = Selection {
        agent_name: Some("build".to_owned()),
        model: Some(ModelChoice {
            provider_id: "provider".to_owned(),
            model_id: "newer".to_owned(),
            variant: Some("low".to_owned()),
        }),
    };
    let parts = captures
        .get(&capture.id)
        .expect("retained capture")
        .rendered_parts("starter instructions".to_owned());
    assert_eq!(parts.files, capture.parts.files);
    assert_eq!(parts.metadata, capture.parts.metadata);
    assert_eq!(parts.context[0].id, capture.id);
    assert_eq!(parts.context[0].text, "starter instructions");
    assert_eq!(&parts.context[1..], capture.parts.context.as_slice());
    assert_eq!(
        captures
            .get(&capture.id)
            .expect("retained capture")
            .selection,
        capture.selection
    );
    assert_ne!(capture.selection, newer_selection);
    let transferred = captures.handoff(&capture.id).expect("creation handoff");
    assert_eq!(transferred, capture);
    assert_eq!(drafts.get(&owner), Some(&newer));
    assert!(captures.can_begin(&owner));
}

#[test]
fn preset_rollback_retains_full_content_and_rebases_source_mentions_in_utf16() {
    let mut original = source("original", "  😀 @plan  ");
    original.content.agents.push(AgentMention {
        name: "plan".to_owned(),
        mention: Some(Mention {
            start: 5,
            end: 10,
            text: "@plan".to_owned(),
        }),
    });
    let capture = capture("activation", owner("a", "/p"), original.clone());
    let rollback = capture
        .rollback_draft()
        .expect("source mention fits the preset");
    assert_eq!(rollback.text, "/explore 😀 @plan");
    assert_eq!(rollback.content.files, original.content.files);
    assert_eq!(rollback.content.context, original.content.context);
    assert_eq!(rollback.content.skills, original.content.skills);
    assert_eq!(
        rollback.content.metadata_json,
        original.content.metadata_json
    );
    let mention = rollback.content.agents[0]
        .mention
        .as_ref()
        .expect("rollback mention");
    assert_eq!((mention.start, mention.end), (12, 17));
    crate::draft_storage::validate(&mut 0, &rollback).expect("valid full rollback");
    let parts = capture.rendered_parts("instructions".to_owned());
    assert_eq!(parts.agents[0].name, "plan");
    assert!(
        parts.agents[0].mention.is_none(),
        "source ranges cannot label rendered text"
    );
    assert_eq!(
        capture.draft, original,
        "rendering never mutates the original source"
    );
}

#[test]
fn a_late_read_restores_its_owner_after_runtime_and_project_switches() {
    let source_owner = owner("old", "/p");
    let active_owner = owner("new", "/other");
    let original = source("original", "original");
    let capture = capture("activation", source_owner.clone(), original.clone());
    let mut drafts = DraftStore::new();
    drafts.save(source_owner.clone(), original.clone());
    let mut starters = crate::starters::Starters::default();
    assert!(starters.captures.begin(capture.clone(), &mut drafts));
    let mut newer_source = source("newer-source", "newer source typing");
    newer_source.content.metadata_json = original.content.metadata_json.clone();
    let active = source("active", "active typing");
    drafts.save(source_owner.clone(), newer_source.clone());
    drafts.save(active_owner.clone(), active.clone());
    starters.reset();
    assert_eq!(starters.captures.snapshot(), vec![capture.clone()]);
    let restored = starters
        .captures
        .restore_draft(&capture.id, &mut drafts)
        .expect("independent content merges")
        .expect("retained original owner");
    assert_eq!(restored.owner, source_owner);
    let merged = drafts
        .get(&source_owner)
        .expect("source restored offscreen");
    assert_eq!(merged.text, "newer source typing\n\noriginal");
    assert_eq!(
        merged.content.files,
        [newer_source.content.files, original.content.files].concat()
    );
    assert_eq!(
        merged.content.context,
        [newer_source.content.context, original.content.context].concat()
    );
    assert_eq!(drafts.get(&active_owner), Some(&active));
    assert!(starters.captures.snapshot().is_empty());
}

#[test]
fn lost_override_reads_drain_after_hydration_without_replaying() {
    let owner = owner("a", "/p");
    let original = source("original", "original typing");
    let capture = capture("activation", owner.clone(), original.clone());
    let mut drafts = DraftStore::new();
    drafts.save(owner.clone(), original.clone());
    let mut captures = StarterCaptures::default();
    assert!(captures.begin(capture.clone(), &mut drafts));
    let snapshot = captures.snapshot();
    let mut restored = StarterCaptures::restore(snapshot);
    let drained = restored.drain(&mut drafts);
    assert!(drained.conflicts.is_empty());
    assert_eq!(drained.restored, vec![capture]);
    assert_eq!(drafts.get(&owner), Some(&original));
    assert!(restored.snapshot().is_empty());
    assert!(
        restored.drain(&mut drafts).restored.is_empty(),
        "restoration is single-use"
    );
}

#[test]
fn failed_metadata_merge_retains_the_original_capture_until_explicit_recovery() {
    let owner = owner("a", "/p");
    let original = source("original", "original");
    let capture = capture("activation", owner.clone(), original.clone());
    let mut drafts = DraftStore::new();
    drafts.save(owner.clone(), original.clone());
    let mut captures = StarterCaptures::default();
    assert!(captures.begin(capture.clone(), &mut drafts));
    let mut newer = source("newer", "newer");
    drafts.save(owner.clone(), newer.clone());
    assert_eq!(
        captures.restore_draft(&capture.id, &mut drafts),
        Err(ContentConflict::Metadata)
    );
    assert_eq!(drafts.get(&owner), Some(&newer));
    assert_eq!(captures.snapshot(), vec![capture.clone()]);
    let mut restored = StarterCaptures::restore(captures.snapshot());
    let drained = restored.drain(&mut drafts);
    assert!(drained.restored.is_empty());
    assert_eq!(
        drained.conflicts,
        vec![(capture.id.clone(), ContentConflict::Metadata)]
    );
    assert_eq!(restored.get(&capture.id), Some(&capture));
    newer.content.metadata_json = original.content.metadata_json;
    drafts.save(owner.clone(), newer);
    assert!(
        restored
            .restore_draft(&capture.id, &mut drafts)
            .expect("resolved conflict")
            .is_some()
    );
    assert!(restored.snapshot().is_empty());
    assert_eq!(
        drafts
            .get(&owner)
            .expect("merged full draft")
            .content
            .files
            .len(),
        2
    );
}

#[test]
fn a_capture_cannot_replace_an_existing_owner_or_take_an_unsaved_draft() {
    let owner = owner("a", "/p");
    let original = source("original", "original");
    let capture = capture("activation", owner.clone(), original.clone());
    let newer = source("newer", "newer");
    let mut drafts = DraftStore::new();
    drafts.save(owner.clone(), newer.clone());
    let mut captures = StarterCaptures::default();
    assert!(!captures.begin(capture.clone(), &mut drafts));
    assert_eq!(drafts.get(&owner), Some(&newer));
    drafts.save(owner.clone(), original);
    assert!(captures.begin(capture.clone(), &mut drafts));
    drafts.save(owner.clone(), newer.clone());
    let replacement = StarterCapture {
        id: MessageId::from("replacement"),
        ..capture.clone()
    };
    assert!(!captures.begin(replacement, &mut drafts));
    assert_eq!(captures.snapshot(), vec![capture]);
    assert_eq!(drafts.get(&owner), Some(&newer));
}

#[cfg(unix)]
#[test]
fn retained_prepared_bytes_survive_source_replacement_and_starter_handoff() {
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::attachment_catalog::{self, AttachmentCatalog, PreparedAttachment};
    use crate::attachment_spool::{AttachmentSpool, BlobId, SpoolCommit};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let scratch = loop {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!(
            "/tmp/opencode/starter-capture-{}-{sequence}",
            std::process::id()
        ));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => break Scratch(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => panic!("create private test profile"),
        }
    };
    let source_path = scratch.0.join("source.txt");
    let bytes = b"original attachment\0\r\n";
    fs::write(&source_path, bytes).expect("write original source");
    let captured_bytes = fs::read(&source_path).expect("capture source once");
    let mut spool = AttachmentSpool::open(&scratch.0).expect("open private spool");
    let blob = BlobId::parse("0123456789abcdef0123456789abcdef").expect("blob identity");
    let receipt = spool
        .put(blob, &captured_bytes, captured_bytes.len())
        .expect("retain original bytes");
    assert_eq!(receipt.commit, SpoolCommit::Durable);
    let owner = owner("a", "/p");
    let mut original = source("original", "original");
    original.content.files[0].content = DraftFileContent::Prepared;
    original.content.files[0].byte_len = Some(bytes.len() as u64);
    let file_id = original.content.files[0].id.clone();
    let mut catalog = AttachmentCatalog::new();
    catalog.insert(
        file_id.clone(),
        PreparedAttachment {
            blob: receipt.descriptor,
            staging: None,
        },
    );
    let capture = capture("activation", owner.clone(), original.clone());
    let mut drafts = DraftStore::new();
    drafts.save(owner.clone(), original);
    let mut captures = StarterCaptures::default();
    assert!(captures.begin(capture.clone(), &mut drafts));
    fs::write(&source_path, b"new source bytes").expect("replace source after activation");
    drafts.save(owner.clone(), source("newer", "newer typing"));
    let restored = StarterCaptures::restore(captures.snapshot());
    let pending = restored.get(&capture.id).expect("pending original capture");
    attachment_catalog::validate_draft(&catalog, &owner, &pending.draft, &mut HashMap::new())
        .expect("pending capture roots the prepared bytes");
    let rollback = pending.rollback_draft().expect("full preset rollback");
    assert_eq!(
        crate::attachment_preparation::prepared_resources(&rollback),
        vec![file_id.clone()]
    );
    let mut restored = restored;
    let transferred = restored
        .handoff(&capture.id)
        .expect("transfer original ownership");
    assert_eq!(transferred.draft.content.files[0].id, file_id);
    assert_eq!(
        spool
            .read(&receipt.descriptor, bytes.len())
            .expect("read retained bytes")
            .as_slice(),
        bytes
    );
    assert_eq!(
        drafts.get(&owner).expect("independent newer draft").text,
        "newer typing"
    );
}
