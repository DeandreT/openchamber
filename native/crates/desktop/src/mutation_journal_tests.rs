use super::*;
use crate::send_attempt::{SendAttempts, tests as attempt_tests};

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
fn journal_v1_migrates_without_losing_existing_sends() {
    let fixture = Scratch::new();
    let path = fixture.path();
    let (mut journal, _) = MutationJournal::open(&path).expect("open");
    journal.save(&sample()).expect("v2 save");
    drop(journal);
    let mut stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("JSON");
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
    assert_eq!(stored["version"], 3);
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
        };
        let stored = Store::capture(&snapshot).expect("capture");
        let restored = stored.into_snapshot().expect("restore");
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
        LoadedJournal::Present(snapshot) => snapshot,
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
        let recovered = Store::capture(&snapshot)
            .expect("valid capture")
            .into_snapshot()
            .expect("valid recovery");
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
    let mut store = Store::capture(&sample()).expect("capture");
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

    let mut store = Store::capture(&sample()).expect("capture");
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
