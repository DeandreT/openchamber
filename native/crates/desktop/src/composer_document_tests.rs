use super::*;
use gpui_kit::AppContext;
use openchamber_native_core::draft::DraftTarget;
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::queue_manifest::{QueueAgent, QueuePromptParts, QueueSkill};

fn owner() -> DraftKey {
    DraftKey::new(
        RuntimeId::from("runtime"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_composer")),
    )
    .expect("fixture directory is absolute")
}

fn reference() -> DraftCompanionRef {
    DraftCompanionRef {
        operation: QueueOperationId::new("composer_take_1").expect("fixture identity"),
        companion: QueueObjectId::new("composer_take_1-destination").expect("fixture identity"),
    }
}

fn key() -> ComposerSourceKey {
    let reference = reference();
    ComposerSourceKey::Companion {
        operation: reference.operation,
        companion: reference.companion,
        item: 0,
    }
}

fn item(content: &str, text: &str) -> QueueLocalItem {
    QueueLocalItem {
        content: content.into(),
        text: text.into(),
        files: Vec::new(),
        send_config: QueuedSendConfig {
            provider_id: "provider".into(),
            model_id: "model".into(),
            agent: None,
            variant: None,
        },
        prompt_parts: QueuePromptParts::default(),
    }
}

fn document(text: &str) -> Document {
    let draft = Draft::new(text);
    Document {
        generation: 1,
        revision: 1,
        text: Rope::from(text),
        selection: text.len()..text.len(),
        layout: Layout::native(&draft),
        known: Vec::new(),
        durable: None,
        read: ReadState::Ready,
        history: HashMap::new(),
        history_order: VecDeque::new(),
        append: None,
        deferred: None,
        planned: None,
        save_failure: None,
    }
}

fn edit_reference(generation: u64, revision: u64) -> ComposerEditRef {
    ComposerEditRef {
        operation: reference().operation,
        object: QueueObjectId::new("composer_edit_1").expect("identity"),
        generation,
        revision,
        byte_len: 20,
        sha256: [0; 32],
        companions: vec![reference()],
    }
}

fn rich_item(content: &str, text: &str) -> QueueLocalItem {
    use openchamber_native_transport::queue_manifest::QueueLocalFile;

    let mut local = item(content, text);
    local.files.push(QueueLocalFile {
        name: "notes.txt".into(),
        mime_type: "text/plain".into(),
        byte_len: 1,
        sha256: sha256(b"x"),
        description: None,
        mention: None,
    });
    local.prompt_parts.agents.push(QueueAgent {
        name: "agent".into(),
        mention: None,
    });
    local.prompt_parts.skills.push(QueueSkill {
        id: "skill".into(),
        mention: None,
    });
    local
}

fn event(
    old: Range<usize>,
    inserted: &str,
    text: &str,
    before: u64,
    after: u64,
) -> CommittedInputEdit {
    CommittedInputEdit {
        replacements: vec![InputReplacement {
            range: old,
            text: inserted.to_owned().into(),
        }],
        before,
        after,
        text: Rope::from(text),
        selection: text.len()..text.len(),
    }
}

fn editing_capture(document: &Document) -> ComposerCapture {
    ComposerCapture {
        composer: CompanionCapture {
            native: QueueNativeSource {
                owner: owner(),
                draft: Draft::default(),
            },
            references: document.known.clone(),
        },
        generation: document.generation,
        revision: document.revision,
        edit: None,
        snapshot: Some((
            document.text.clone(),
            document.selection.clone(),
            document.layout.clone(),
            document.known.clone(),
        )),
        planned: document.planned.clone(),
        close: None,
        explicit_retry: false,
        native_authority: None,
    }
}

#[test]
fn confirmed_deletion_forgets_only_exact_session_documents_and_returns_retained_native_roots() {
    use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};

    let deleted = owner();
    let other_session = DraftKey::new(
        deleted.runtime().clone(),
        "/project",
        DraftTarget::Session(SessionId::from("ses_kept")),
    )
    .expect("other session");
    let other_runtime = DraftKey::new(
        RuntimeId::from("other-runtime"),
        "/project",
        deleted.target().clone(),
    )
    .expect("other runtime");
    let old_directory = DraftKey::new(
        deleted.runtime().clone(),
        "/previous-project",
        deleted.target().clone(),
    )
    .expect("same deleted session's previous scope");
    let mut documents = ComposerDocuments {
        epoch: 7,
        frozen: Some(7),
        ..ComposerDocuments::default()
    };
    let mut removed = document("retained failed snapshot");
    let mut capture = editing_capture(&removed);
    let file = AttachmentId::from("00000000000000000000000000000001");
    let group = AttachmentId::from("retained-native-group");
    capture.composer.native.draft.content.files.push(DraftFile {
        id: file.clone(),
        name: Some("retained.txt".into()),
        media_type: Some("text/plain".into()),
        byte_len: Some(1),
        source_document_id: Some(group.clone()),
        content: DraftFileContent::Prepared,
    });
    removed.save_failed(capture, QueueCaptureFailure::Persistence);
    documents.documents.insert(deleted.clone(), removed);
    documents.generations.insert(deleted.clone(), 3);
    documents.native_revisions.insert(deleted.clone(), 9);
    documents.loads.insert(deleted.clone(), Task::ready(()));
    documents.saves.insert(deleted.clone(), Task::ready(()));
    documents
        .loads
        .insert(old_directory.clone(), Task::ready(()));
    for kept in [&other_session, &other_runtime] {
        documents
            .documents
            .insert(kept.clone(), document("independent input"));
        documents.generations.insert(kept.clone(), 5);
        documents.native_revisions.insert(kept.clone(), 11);
        documents.loads.insert(kept.clone(), Task::ready(()));
        documents.saves.insert(kept.clone(), Task::ready(()));
    }

    let roots =
        documents.forget_deleted_sessions(deleted.runtime(), &[SessionId::from("ses_composer")]);
    assert!(roots.contains(&file) && roots.contains(&group));
    assert_eq!(
        roots.len(),
        2,
        "only captured native file and group identities can become cleanup candidates"
    );
    assert!(!documents.documents.contains_key(&deleted));
    assert!(!documents.loads.contains_key(&deleted) && !documents.saves.contains_key(&deleted));
    assert!(!documents.native_revisions.contains_key(&deleted));
    assert_eq!(
        documents.generations.get(&deleted),
        Some(&4),
        "old callbacks cannot match a replacement generation"
    );
    assert!(!documents.loads.contains_key(&old_directory));
    assert_eq!(documents.generations.get(&old_directory), Some(&2));
    assert_eq!(
        (documents.epoch, documents.frozen),
        (7, Some(7)),
        "another owner's close permits retain their window epoch"
    );
    for kept in [&other_session, &other_runtime] {
        assert_eq!(
            documents.documents[kept].text.to_string(),
            "independent input"
        );
        assert!(documents.loads.contains_key(kept) && documents.saves.contains_key(kept));
        assert_eq!(documents.generations.get(kept), Some(&5));
        assert_eq!(documents.native_revisions.get(kept), Some(&11));
    }
    assert!(
        documents
            .forget_deleted_sessions(deleted.runtime(), &[SessionId::from("ses_composer")])
            .is_empty(),
        "repeated confirmation creates no additional cleanup authority"
    );
}

#[test]
fn append_undo_and_redo_restore_source_ownership_without_native_projection() {
    let mut document = document("typed");
    let before = document.layout.clone();
    let mut after = before.clone();
    after.spans.push(Span {
        kind: SpanKind::Literal { edited: false },
        bytes: 2,
    });
    after.spans.push(Span {
        kind: SpanKind::Source(key()),
        bytes: 6,
    });
    after
        .sources
        .push(SourceState::local(key(), &item("queued", "delivered")));
    document.known.push(reference());
    document.append = Some(PendingAppend {
        before,
        after,
        suffix: "\n\nqueued".into(),
    });
    document
        .changed(&event(5..5, "\n\nqueued", "typed\n\nqueued", 10, 11))
        .expect("append commits");
    assert_eq!(
        document
            .layout
            .display(&document.text, &ComposerSourceKey::Native),
        "typed"
    );
    assert_eq!(document.layout.display(&document.text, &key()), "queued");
    document
        .changed(&event(5..13, "", "typed", 11, 10))
        .expect("undo commits");
    assert_eq!(document.layout.display(&document.text, &key()), "");
    assert_eq!(
        document.known.len(),
        1,
        "undo text retains the original rich source"
    );
    document
        .changed(&event(5..5, "\n\nqueued", "typed\n\nqueued", 10, 11))
        .expect("redo commits");
    assert_eq!(document.layout.display(&document.text, &key()), "queued");
    assert!(
        !document
            .layout
            .sources
            .last()
            .expect("queued source")
            .edited,
        "redo restores captured delivered text"
    );
}

#[test]
fn empty_display_semantic_append_survives_text_undo_redo_and_keeps_later_removals() {
    let second = DraftCompanionRef {
        operation: QueueOperationId::new("composer_take_2").expect("identity"),
        companion: QueueObjectId::new("composer_take_2-destination").expect("identity"),
    };
    let second_key = ComposerSourceKey::Companion {
        operation: second.operation.clone(),
        companion: second.companion.clone(),
        item: 0,
    };
    let local = rich_item("", "untouched semantic delivery");
    let mut document = document("n");
    document.stage_loaded(Loaded {
        snapshot: None,
        headers: vec![(
            key(),
            "A".into(),
            SourceState::local(key(), &item("A", "A")),
        )],
        references: vec![reference()],
    });
    document
        .changed(&event(1..1, "\n\nA", "n\n\nA", 10, 11))
        .expect("existing compound source");
    document
        .changed(&event(4..4, "!", "n\n\nA!", 11, 12))
        .expect("typing before semantic-only append");
    document.stage_loaded(Loaded {
        snapshot: None,
        headers: vec![(
            second_key.clone(),
            String::new(),
            SourceState::local(second_key.clone(), &local),
        )],
        references: vec![reference(), second],
    });
    assert!(
        document.append.is_none(),
        "an empty display appends no toolkit text transaction"
    );
    assert!(document.layout.contains_source(&second_key));
    for edit in [
        event(4..5, "", "n\n\nA", 12, 11),
        event(4..4, "!", "n\n\nA!", 11, 12),
    ] {
        document
            .changed(&edit)
            .expect("unrelated text undo or redo");
        let state = document
            .layout
            .sources
            .iter()
            .find(|source| source.key == second_key)
            .expect("semantic source remains owned");
        let mut projected = local.clone();
        let reprocess = state
            .project_display(
                &mut projected,
                &document.layout.display(&document.text, &second_key),
                document.layout.contains_source(&second_key),
            )
            .expect("semantic-only source projection");
        assert!(
            !reprocess,
            "text history cannot turn an untouched semantic-only source into deleted display text"
        );
        assert_eq!(projected.text, "untouched semantic delivery");
        let order: Vec<_> = document
            .layout
            .spans
            .iter()
            .filter_map(|span| match &span.kind {
                SpanKind::Source(key) => Some(key.clone()),
                SpanKind::Literal { .. } => None,
            })
            .collect();
        assert!(
            order == [ComposerSourceKey::Native, key(), second_key.clone()],
            "semantic-only appends retain their source order"
        );
    }
    let state = document
        .layout
        .sources
        .iter_mut()
        .find(|source| source.key == second_key)
        .expect("exact semantic source");
    state.removed_files.push(0);
    state.removed_agents.push(0);
    state.removed_skills.push(0);
    document.revision += 1;
    document
        .changed(&event(4..5, "", "n\n\nA", 12, 11))
        .expect("text undo after explicit reference removal");
    document
        .changed(&event(4..4, "!", "n\n\nA!", 11, 12))
        .expect("text redo after explicit reference removal");
    let state = document
        .layout
        .sources
        .iter()
        .find(|source| source.key == second_key)
        .expect("retained semantic source");
    let mut projected = local;
    assert!(
        !state
            .project_display(
                &mut projected,
                "",
                document.layout.contains_source(&second_key)
            )
            .expect("removed reference projection")
    );
    assert_eq!(projected.text, "untouched semantic delivery");
    assert!(
        projected.files.is_empty()
            && projected.prompt_parts.agents.is_empty()
            && projected.prompt_parts.skills.is_empty(),
        "preserving the empty source cannot resurrect explicitly removed references"
    );
}

#[test]
fn cross_source_replacement_preserves_deleted_separator_and_all_source_slots() {
    let mut document = document("abc\n\ndef");
    document.layout.spans = vec![
        Span {
            kind: SpanKind::Source(ComposerSourceKey::Native),
            bytes: 3,
        },
        Span {
            kind: SpanKind::Literal { edited: false },
            bytes: 2,
        },
        Span {
            kind: SpanKind::Source(key()),
            bytes: 3,
        },
    ];
    document
        .layout
        .sources
        .push(SourceState::local(key(), &item("def", "def")));
    document
        .changed(&event(1..7, "X", "aXf", 1, 2))
        .expect("cross-source replacement commits");
    assert_eq!(
        document
            .layout
            .display(&document.text, &ComposerSourceKey::Native),
        "aX"
    );
    assert_eq!(document.layout.display(&document.text, &key()), "f");
    assert_eq!(
        document.layout.spans[1].bytes, 0,
        "send projection cannot restore the deleted separator"
    );
    assert!(document.layout.sources.iter().all(|source| source.edited));
}

#[test]
fn exact_unicode_replacement_shifts_surviving_file_agent_and_skill_anchors() {
    let mut local = item("😀 @agent", "😀 @agent");
    local.prompt_parts.agents.push(QueueAgent {
        name: "agent".into(),
        mention: Some(QueueMention {
            start: 3,
            end: 9,
            text: "@agent".into(),
        }),
    });
    local.prompt_parts.skills.push(QueueSkill {
        id: "skill".into(),
        mention: Some(QueueMention {
            start: 3,
            end: 9,
            text: "@agent".into(),
        }),
    });
    let mut state = SourceState::local(key(), &local);
    let cloned = state.clone();
    assert!(
        Arc::ptr_eq(
            &state.agents[0].as_ref().expect("anchor").text,
            &cloned.agents[0].as_ref().expect("anchor").text
        ),
        "undo layouts share immutable mention text"
    );
    state.edit("😀 @agent", 0..0, "é");
    assert_eq!(
        state.agents[0]
            .as_ref()
            .map(|anchor| (anchor.start, anchor.end)),
        Some((4, 10))
    );
    assert_eq!(
        state.skills[0]
            .as_ref()
            .map(|anchor| (anchor.start, anchor.end)),
        Some((4, 10))
    );
    state.edit("é😀 @agent", 8..9, "x");
    assert!(
        state.agents[0].is_none(),
        "editing inside the mention clears its position"
    );
    state.project(&mut local).expect("semantic projection");
    assert_eq!(local.prompt_parts.agents[0].name, "agent");
    assert_eq!(local.prompt_parts.skills[0].id, "skill");
}

#[test]
fn different_display_and_delivered_text_never_uses_a_guessed_anchor_mapping() {
    let mut local = item("#snippet @agent", "expanded arbitrary text @agent");
    local.prompt_parts.agents.push(QueueAgent {
        name: "agent".into(),
        mention: Some(QueueMention {
            start: 24,
            end: 30,
            text: "@agent".into(),
        }),
    });
    let mut state = SourceState::local(key(), &local);
    state.edit("#snippet @agent", 0..0, "new ");
    state.project(&mut local).expect("semantic projection");
    assert!(local.prompt_parts.agents[0].mention.is_none());
    assert_eq!(local.prompt_parts.agents[0].name, "agent");
    assert_eq!(
        local.text, "expanded arbitrary text @agent",
        "normal preparation owns rebuilding delivered text"
    );
}

#[test]
fn removal_exclusions_keep_original_occurrence_and_do_not_remove_equal_siblings() {
    let mut local = item("@agent", "@agent");
    let agent = QueueAgent {
        name: "agent".into(),
        mention: Some(QueueMention {
            start: 0,
            end: 6,
            text: "@agent".into(),
        }),
    };
    local.prompt_parts.agents = vec![agent.clone(), agent];
    let mut state = SourceState::local(key(), &local);
    state.removed_agents.push(0);
    let exclusions = state.exclusions(2, &local);
    state
        .project(&mut local)
        .expect("exact occurrence projection");
    assert_eq!(
        local.prompt_parts.agents.len(),
        1,
        "same-ID retained occurrence survives"
    );
    assert_eq!(exclusions.source, 2);
    assert_eq!(exclusions.agents[0].ordinal, 0);
    assert_eq!(exclusions.agents[0].identity, "agent");
    assert_eq!(
        exclusions.agents[0]
            .original_mention
            .as_ref()
            .map(|mention| mention.start),
        Some(0)
    );
}

#[test]
fn reference_only_removals_preserve_the_captured_snippet_expansion() {
    let mut local = rich_item("#snippet", "captured expansion v1");
    let mut state = SourceState::local(key(), &local);
    state.removed_files.push(0);
    state.removed_agents.push(0);
    state.removed_skills.push(0);
    assert!(
        !state.edited,
        "reference controls do not edit the displayed text"
    );
    assert!(
        !state
            .project_display(&mut local, "#snippet", true)
            .expect("exact projection"),
        "reference-only projection must skip mention/snippet preparation"
    );
    assert_eq!(local.text, "captured expansion v1");
    assert!(
        local.files.is_empty()
            && local.prompt_parts.agents.is_empty()
            && local.prompt_parts.skills.is_empty()
    );
}

#[test]
fn removing_the_final_attachment_clears_sendability() {
    let mut state = SourceState::local(key(), &rich_item("", ""));
    assert!(state.has_sendable_references());
    state.removed_files.push(0);
    assert!(
        !state.has_sendable_references(),
        "agent and skill references alone do not make a blank prompt sendable"
    );
}

#[test]
fn removals_survive_undo_before_append_and_redo_without_a_visible_source() {
    let mut document = document("typed");
    let mut local = rich_item("queued", "expanded v1");
    local.prompt_parts.file_references.push(
        openchamber_native_transport::queue_manifest::QueueFileReference {
            uri: "file:///project/notes.pdf".into(),
            name: "notes.pdf".into(),
            description: None,
            mention: None,
        },
    );
    document.stage_loaded(Loaded {
        snapshot: None,
        headers: vec![(key(), "queued".into(), SourceState::local(key(), &local))],
        references: vec![reference()],
    });
    document
        .changed(&event(5..5, "\n\nqueued", "typed\n\nqueued", 10, 11))
        .expect("append");
    let state = document
        .layout
        .sources
        .iter_mut()
        .find(|source| source.key == key())
        .expect("queued source");
    state.removed_files.push(0);
    state.removed_file_references.push(0);
    state.removed_agents.push(0);
    state.removed_skills.push(0);
    document
        .changed(&event(5..13, "", "typed", 11, 10))
        .expect("undo append");
    assert!(!document.layout.contains_source(&key()));
    let state = document
        .layout
        .sources
        .iter()
        .find(|source| source.key == key())
        .expect("absent text retains its exclusions");
    let mut absent = local.clone();
    assert!(
        state
            .project_display(&mut absent, "", false)
            .expect("deleted text projects")
    );
    assert!(absent.text.is_empty() && absent.files.is_empty());
    assert!(absent.prompt_parts.agents.is_empty() && absent.prompt_parts.skills.is_empty());
    assert!(absent.prompt_parts.file_references.is_empty());
    assert_eq!(state.exclusions(1, &local).agents.len(), 1);
    assert_eq!(state.exclusions(1, &local).file_references.len(), 1);
    let encoded =
        serde_json::to_vec(&document.layout).expect("exclusions persist without a source span");
    document.layout = serde_json::from_slice(&encoded).expect("source state round-trip");
    document
        .changed(&event(5..5, "\n\nqueued", "typed\n\nqueued", 10, 11))
        .expect("redo append");
    let state = document
        .layout
        .sources
        .iter()
        .find(|source| source.key == key())
        .expect("restored source");
    let mut restored = local;
    assert!(
        !state
            .project_display(&mut restored, "queued", true)
            .expect("redo source projects")
    );
    assert_eq!(restored.text, "expanded v1");
    assert!(
        restored.files.is_empty()
            && restored.prompt_parts.agents.is_empty()
            && restored.prompt_parts.skills.is_empty()
    );
    assert!(restored.prompt_parts.file_references.is_empty());
}

#[test]
fn file_reference_occurrences_rebase_and_remove_independently_of_owned_byte_files() {
    use openchamber_native_transport::queue_manifest::QueueFileReference;

    let old = "@a.pdf @a.pdf";
    let mut local = rich_item(old, old);
    local.prompt_parts.file_references = [0, 7]
        .into_iter()
        .map(|start| QueueFileReference {
            uri: "file:///project/a.pdf".into(),
            name: "a.pdf".into(),
            description: Some("reference only".into()),
            mention: Some(QueueMention {
                start,
                end: start + 6,
                text: "@a.pdf".into(),
            }),
        })
        .collect();
    let mut state = SourceState::local(key(), &local);
    state.edit(old, 0..0, "😀 ");
    state.removed_file_references.push(0);
    let excluded = state.exclusions(1, &local);
    assert_eq!(
        excluded.file_references[0].identity,
        "file:///project/a.pdf"
    );
    assert_eq!(excluded.file_references[0].ordinal, 0);
    assert_eq!(
        excluded.file_references[0]
            .display_mention
            .as_ref()
            .expect("shifted occurrence")
            .start,
        3
    );
    state
        .project_display(&mut local, "😀 @a.pdf @a.pdf", true)
        .expect("reference projection");
    assert_eq!(
        local.files.len(),
        1,
        "a reference removal cannot consume an owned byte-file ordinal"
    );
    assert_eq!(
        local.prompt_parts.file_references.len(),
        1,
        "equal-URI siblings remain separate occurrences"
    );
    let retained = &local.prompt_parts.file_references[0];
    assert_eq!(
        retained.mention.as_ref().expect("surviving anchor").start,
        10
    );
    assert_eq!(retained.description.as_deref(), Some("reference only"));
}

#[test]
fn old_source_layouts_default_the_new_reference_arrays_without_changing_projection() {
    let mut local = rich_item("queued", "captured delivery");
    let state = SourceState::local(key(), &local);
    let encoded = serde_json::to_value(&state).expect("source metadata");
    assert!(
        encoded.get("file_references").is_none()
            && encoded.get("removed_file_references").is_none(),
        "empty new arrays preserve the previous saved source shape"
    );
    let restored: SourceState = serde_json::from_value(encoded)
        .expect("older source layouts restore with empty reference arrays");
    assert!(
        !restored
            .project_display(&mut local, "queued", true)
            .expect("original source semantics")
    );
    assert_eq!(local.files.len(), 1);
    assert_eq!(local.text, "captured delivery");
}

#[test]
fn a_reference_only_source_stops_being_sendable_after_its_last_reference_is_removed() {
    use openchamber_native_transport::queue_manifest::QueueFileReference;

    let mut local = item("", "");
    local.prompt_parts.file_references.push(QueueFileReference {
        uri: "file:///project/a.pdf".into(),
        name: "a.pdf".into(),
        description: None,
        mention: None,
    });
    let mut state = SourceState::local(key(), &local);
    assert!(state.has_sendable_references());
    state.removed_file_references.push(0);
    assert!(!state.has_sendable_references());
    assert!(
        !state
            .project_display(&mut local, "", true)
            .expect("reference-only removal preserves delivery")
    );
    assert!(local.files.is_empty() && local.prompt_parts.file_references.is_empty());
}

#[test]
fn hydration_reconciles_saved_edits_and_removals_with_typing_during_the_read() {
    let local = rich_item("queued", "expanded v1");
    let mut state = SourceState::local(key(), &local);
    state.edited = true;
    state.removed_files.push(0);
    let snapshot = EditDocument {
        version: 1,
        owner: Owner::capture(&owner()),
        generation: 1,
        revision: 7,
        companions: vec![reference()],
        shown: vec![reference()],
        text: "edited".into(),
        start: 2,
        end: 4,
        layout: Layout {
            spans: vec![Span {
                kind: SpanKind::Source(key()),
                bytes: 6,
            }],
            sources: vec![state],
        },
    };
    let mut document = document("new");
    document.read = ReadState::Loading;
    document.stage_loaded(Loaded {
        snapshot: Some(snapshot),
        headers: vec![(
            key(),
            local.content.clone(),
            SourceState::local(key(), &local),
        )],
        references: vec![reference()],
    });
    assert!(
        !document.ready_for_capture(&[reference()]),
        "the toolkit append must commit before close can freeze"
    );
    document
        .changed(&event(3..3, "\n\nedited", "new\n\nedited", 1, 2))
        .expect("reconciled append");
    assert_eq!(document.text.to_string(), "new\n\nedited");
    assert_eq!(document.layout.display(&document.text, &key()), "edited");
    let state = document
        .layout
        .sources
        .iter()
        .find(|source| source.key == key())
        .expect("saved source");
    assert!(state.edited && state.removed_files == [0]);
    assert!(document.ready_for_capture(&[reference()]));
}

#[test]
fn a_completed_subset_read_requires_reconciling_the_latest_take_references() {
    let second = DraftCompanionRef {
        operation: QueueOperationId::new("composer_take_2").expect("identity"),
        companion: QueueObjectId::new("composer_take_2-destination").expect("identity"),
    };
    let second_key = ComposerSourceKey::Companion {
        operation: second.operation.clone(),
        companion: second.companion.clone(),
        item: 0,
    };
    let mut document = document("");
    document.stage_loaded(Loaded {
        snapshot: None,
        headers: vec![(
            key(),
            "A".into(),
            SourceState::local(key(), &item("A", "A")),
        )],
        references: vec![reference()],
    });
    document
        .changed(&event(0..0, "A", "A", 1, 2))
        .expect("first read commits");
    let latest = vec![reference(), second.clone()];
    assert!(
        !document.ready_for_capture(&latest),
        "a Take added during A's read remains missing"
    );
    document.stage_loaded(Loaded {
        snapshot: None,
        headers: vec![(
            second_key.clone(),
            "B".into(),
            SourceState::local(second_key.clone(), &item("B", "B")),
        )],
        references: latest.clone(),
    });
    document
        .changed(&event(1..1, "\n\nB", "A\n\nB", 2, 3))
        .expect("follow-up read commits");
    assert!(document.ready_for_capture(&latest));
    assert_eq!(document.layout.display(&document.text, &key()), "A");
    assert_eq!(document.layout.display(&document.text, &second_key), "B");
}

#[test]
fn selection_only_changes_need_a_new_durable_revision() {
    let mut document = document("typed");
    document.durable = Some(edit_reference(1, 1));
    document.select(1..3);
    assert_eq!(
        document.revision, 2,
        "caret state is part of the persisted editing snapshot"
    );
    assert_eq!(document.selection, 1..3);
    assert_eq!(
        document
            .durable
            .as_ref()
            .expect("old snapshot retained")
            .revision,
        1
    );
    document.select(1..3);
    assert_eq!(document.revision, 2, "an unchanged selection is a no-op");
}

#[test]
fn failed_autosave_keeps_the_exact_snapshot_and_planned_put_for_explicit_retry() {
    let mut document = document("first");
    document.known.push(reference());
    let planned = edit_reference(1, 1);
    document.planned = Some(planned.clone());
    let capture = ComposerCapture {
        composer: CompanionCapture {
            native: QueueNativeSource {
                owner: owner(),
                draft: Draft::default(),
            },
            references: vec![reference()],
        },
        generation: 1,
        revision: 1,
        edit: None,
        snapshot: Some((
            document.text.clone(),
            1..3,
            document.layout.clone(),
            document.known.clone(),
        )),
        planned: None,
        close: None,
        explicit_retry: false,
        native_authority: None,
    };
    document
        .changed(&event(5..5, " later", "first later", 1, 2))
        .expect("typing during autosave");
    document.save_failed(capture, QueueCaptureFailure::Persistence);
    assert!(
        !document.ready_for_capture(&[reference()]),
        "normal autosave and submit cannot repair a failed put"
    );
    let failure = document
        .save_failure
        .as_ref()
        .expect("failure remains visible");
    assert!(matches!(failure.error, QueueCaptureFailure::Persistence));
    assert_eq!(failure.capture.revision, 1);
    assert!(failure.capture.planned.as_ref() == Some(&planned));
    assert!(!failure.capture.explicit_retry);
    let retry = failure.capture.clone().for_explicit_retry();
    assert!(retry.explicit_retry);
    let (text, selection, _, _) = retry.snapshot.expect("the failed snapshot is retained");
    assert_eq!(text.to_string(), "first");
    assert_eq!(selection, 1..3);
    assert_eq!(
        document.text.to_string(),
        "first later",
        "repair never replaces newer typing"
    );
}

#[test]
fn failed_put_keeps_its_accepted_plan_after_a_newer_save_replaces_the_document_slot() {
    let mut document = document("first");
    document.known.push(reference());
    let original_plan = edit_reference(1, 1);
    document.planned = Some(original_plan.clone());
    let failed = editing_capture(&document);
    document
        .changed(&event(5..5, " later", "first later", 1, 2))
        .expect("typing while the original put is outstanding");
    let mut newer_plan = edit_reference(1, 2);
    newer_plan.object = QueueObjectId::new("composer_edit_2").expect("independent newer object");
    document.planned = Some(newer_plan.clone());
    document.save_failed(failed, QueueCaptureFailure::Persistence);
    let failure = document
        .save_failure
        .as_ref()
        .expect("the original immutable put still needs explicit repair");
    assert!(failure.capture.planned.as_ref() == Some(&original_plan));
    assert!(
        document.planned.as_ref() == Some(&newer_plan),
        "failure recording preserves the newer plan independently"
    );
    document
        .changed(&event(11..11, "!", "first later!", 2, 3))
        .expect("further typing");
    assert!(
        !document.ready_for_capture(&[reference()]),
        "editing cannot discard an accepted partial-put intent"
    );
    let retry = document
        .save_failure
        .as_ref()
        .expect("retained original repair")
        .capture
        .clone()
        .for_explicit_retry();
    assert!(retry.planned.as_ref() == Some(&original_plan) && retry.explicit_retry);
}

#[test]
fn undoing_an_oversized_unwritten_snapshot_allows_a_corrected_revision_to_save() {
    use crate::queue_capture::{QUEUE_CAPTURE_BYTES, QueueCaptureBytes};

    let mut document = document("small");
    document.known.push(reference());
    let oversized = "x".repeat(QUEUE_CAPTURE_BYTES as usize);
    document
        .changed(&event(0..5, &oversized, &oversized, 1, 2))
        .expect("large paste remains editable");
    let capture = editing_capture(&document);
    let snapshot = EditDocument {
        version: 1,
        owner: Owner::capture(&owner()),
        generation: document.generation,
        revision: document.revision,
        companions: document.known.clone(),
        shown: document.known.clone(),
        text: document.text.to_string(),
        start: document.selection.start,
        end: document.selection.end,
        layout: document.layout.clone(),
    };
    let encoded =
        serde_json::to_vec(&snapshot).expect("valid text can exceed the encoded spool bound");
    assert!(encoded.len() as u64 > QUEUE_CAPTURE_BYTES);
    drop(snapshot);
    let labels = (0..encoded.len().div_ceil(QUEUE_CHUNK_BYTES))
        .map(|ordinal| {
            BlobId::parse(&format!("{:032x}", ordinal + 1)).expect("complete test chunk labels")
        })
        .collect::<Vec<_>>();
    assert!(
        QueueCaptureBytes::new(QueueObjectKind::NativeCompanion, encoded, &labels).is_err(),
        "the encoded snapshot is refused before any chunk intent or put"
    );
    assert!(document.planned.is_none());
    document.save_failed(capture, QueueCaptureFailure::Invalid);
    document
        .changed(&event(0..oversized.len(), "small", "small", 2, 1))
        .expect("Undo corrects the unsavable paste");
    assert_eq!(document.text.to_string(), "small");
    assert!(document.dirty());
    assert!(
        document.ready_for_capture(&[reference()]),
        "a pre-write refusal must not force Retry of the obsolete oversized bytes after correction"
    );
    let corrected = editing_capture(&document);
    assert_eq!(
        corrected
            .snapshot
            .as_ref()
            .expect("corrected capture")
            .0
            .to_string(),
        "small"
    );
    assert!(
        corrected.planned.is_none(),
        "no partial put needs immutable repair"
    );
}

#[test]
fn obsolete_flush_failure_cannot_poison_a_newer_durable_revision() {
    let mut document = document("first");
    document.generation = 7;
    document.revision = 18;
    document.known.push(reference());
    document.planned = Some(edit_reference(7, 18));
    let older = editing_capture(&document);
    document
        .changed(&event(5..5, " later", "first later", 1, 2))
        .expect("typing during the older submission flush");
    let newer = edit_reference(7, 19);
    document.durable = Some(newer.clone());
    document.planned = None;
    assert!(!document.dirty());
    document.save_failed(older, QueueCaptureFailure::Invalid);
    assert_eq!((document.generation, document.revision), (7, 19));
    assert!(
        document.durable.as_ref() == Some(&newer),
        "the newer completed autosave remains authoritative"
    );
    assert_eq!(document.text.to_string(), "first later");
    assert!(
        document.ready_for_capture(&[reference()]),
        "a late obsolete publication refusal cannot require retrying a revision already superseded durably"
    );
    assert!(document.save_failure.is_none());
    let mut prior_generation = editing_capture(&document);
    prior_generation.generation = 6;
    prior_generation.revision = 200;
    prior_generation.planned = Some(edit_reference(6, 200));
    document.save_failed(prior_generation, QueueCaptureFailure::Persistence);
    assert!(
        document.ready_for_capture(&[reference()]) && document.save_failure.is_none(),
        "an old generation cannot replace the current generation's durable state"
    );
}

#[test]
fn edited_display_above_the_native_limit_can_be_materialized_locally() {
    let mut local = item("queued", "expanded v1");
    let mut state = SourceState::local(key(), &local);
    let display = "x".repeat(crate::mutation_journal::MAX_TEXT + 1);
    state.edit("queued", 0..6, &display);
    assert!(
        state
            .project_display(&mut local, &display, true)
            .expect("edited projection")
    );
    let capture = QueueLocalCapture::new("/project", local, Vec::new(), sha256)
        .expect("local display uses the private capture bound");
    assert_eq!(capture.item().content.len(), display.len());
}

#[test]
fn preparation_only_projection_keeps_a_26_mib_display_without_duplicating_it_as_delivery() {
    let mut local = item("old", "old delivered expansion");
    let mut state = SourceState::local(key(), &local);
    let display = "r".repeat(26 * 1024 * 1024);
    state.edit("old", 0..3, &display);
    assert!(
        state
            .project_display(&mut local, &display, true)
            .expect("edited source requires preparation")
    );
    let intermediate = QueueLocalCapture::new("/project", local, Vec::new(), sha256).expect(
        "the intermediate capture contains the display once and fits the unchanged capture bound",
    );
    assert_eq!(intermediate.item().content, display);
    assert!(
        intermediate.item().text.is_empty(),
        "delivered text is an unprepared placeholder until the preparer rebuilds it"
    );
    let mut finished = intermediate.item().clone();
    finished.text = display;
    assert!(
        QueueLocalCapture::new("/project", finished, Vec::new(), sha256).is_err(),
        "a finished payload exceeding 50 MiB still fails the existing encoded limit"
    );
}

#[test]
fn edited_projection_cannot_promote_a_cleared_anchor_from_original_delivery() {
    let original_mention = QueueMention {
        start: 4,
        end: 10,
        text: "@agent".into(),
    };
    let mut local = item("old", "pad @agent");
    local.prompt_parts.agents.push(QueueAgent {
        name: "agent".into(),
        mention: Some(original_mention.clone()),
    });
    let mut state = SourceState::local(key(), &local);
    state.removed_agents.push(0);
    state.edit("old", 0..3, "pad @agent");
    let exclusions = state.exclusions(1, &local);
    assert_eq!(
        exclusions.agents[0].original_mention,
        Some(original_mention)
    );
    assert!(
        exclusions.agents[0].display_mention.is_none(),
        "the old delivered offsets have no proven display mapping"
    );
    assert!(
        state
            .project_display(&mut local, "pad @agent", true)
            .expect("replacement source needs preparation")
    );
    let projected = QueueLocalCapture::new("/project", local, Vec::new(), sha256)
        .expect("edited source projection");
    assert!(
        projected.item().prompt_parts.agents.is_empty(),
        "the exact original occurrence was removed"
    );
    assert_ne!(
        projected.item().content,
        projected.item().text,
        "a provisional delivery must not grant the preparer's content-equals-text fallback for an unrelated new @agent at the old offsets"
    );
    assert!(
        projected.item().text.is_empty(),
        "the original anchor remains historical until preparation proves a position"
    );
}

#[test]
fn edited_source_materializes_without_counting_its_obsolete_large_expansion() {
    let delivered = "e".repeat(48 * 1024 * 1024);
    let original =
        QueueLocalCapture::new("/project", item("#snippet", &delivered), Vec::new(), sha256)
            .expect("the immutable original source fits the capture bound");
    drop(delivered);
    let display = "r".repeat(3 * 1024 * 1024);
    let mut state = SourceState::local(key(), original.item());
    state.edit("#snippet", 0..8, &display);
    let mut projected = original.item().clone();
    assert!(
        state
            .project_display(&mut projected, &display, true)
            .expect("plain-text replacement requires preparation")
    );
    let replacement = QueueLocalCapture::new(original.directory(), projected, Vec::new(), sha256)
        .expect("replacement materialization must fit before normal preparation can replace the old expansion");
    assert_eq!(replacement.item().content, display);
    assert!(
        replacement.item().text.is_empty(),
        "edited sources carry an empty preparation-only delivery placeholder"
    );
    assert_eq!(
        original.item().text.len(),
        48 * 1024 * 1024,
        "the original immutable delivery remains separately retained"
    );
}

#[test]
fn editing_snapshot_exceeds_native_text_limit_while_journal_reference_stays_compact() {
    let text = "x".repeat(crate::mutation_journal::MAX_TEXT + 1);
    let snapshot = EditDocument {
        version: 1,
        owner: Owner::capture(&owner()),
        generation: 7,
        revision: 19,
        companions: vec![reference()],
        shown: Vec::new(),
        start: 0,
        end: 0,
        layout: Layout {
            spans: vec![Span {
                kind: SpanKind::Source(ComposerSourceKey::Native),
                bytes: text.len(),
            }],
            sources: vec![SourceState::native(&Draft::default())],
        },
        text,
    };
    let bytes = serde_json::to_vec(&snapshot).expect("editing snapshot serializes");
    let reference = ComposerEditRef {
        operation: reference().operation,
        object: QueueObjectId::new("composer_edit_1").expect("identity"),
        generation: 7,
        revision: 19,
        byte_len: bytes.len() as u64,
        sha256: sha256(&bytes),
        companions: vec![reference()],
    };
    snapshot
        .validate(&owner(), &reference)
        .expect("full editing text stays outside native Draft validation");
    assert!(bytes.len() > crate::mutation_journal::MAX_TEXT);
    assert!(
        serde_json::to_vec(&reference)
            .expect("compact reference")
            .len()
            < 2048
    );
    let round_trip: EditDocument =
        serde_json::from_slice(&bytes).expect("editing snapshot restores");
    round_trip
        .validate(&owner(), &reference)
        .expect("exact owner and seal");
    assert!(
        round_trip
            .validate(
                &DraftKey::new(
                    RuntimeId::from("other"),
                    "/project",
                    DraftTarget::Session(SessionId::from("ses_composer"))
                )
                .expect("owner"),
                &reference
            )
            .is_err()
    );
}

#[test]
fn close_permit_rejects_other_owner_revision_generation_and_revoked_epoch() {
    let mut reference = ComposerEditRef {
        operation: reference().operation,
        object: QueueObjectId::new("composer_edit_1").expect("identity"),
        generation: 7,
        revision: 19,
        byte_len: 20,
        sha256: [0; 32],
        companions: vec![reference()],
    };
    let permit = ComposerClosePermit {
        owner: owner(),
        epoch: 4,
        generation: 7,
        revision: 19,
        reference: Some(reference.clone()),
    };
    assert!(permit.matches(&owner(), &reference, Some(4)));
    assert!(
        !permit.matches(&owner(), &reference, None),
        "unfreeze revokes the permit"
    );
    assert!(
        !permit.matches(&owner(), &reference, Some(5)),
        "a later close cannot reuse the old epoch"
    );
    reference.revision += 1;
    assert!(!permit.matches(&owner(), &reference, Some(4)));
    reference.revision -= 1;
    reference.generation += 1;
    assert!(!permit.matches(&owner(), &reference, Some(4)));
    reference.generation -= 1;
    let foreign = DraftKey::new(
        RuntimeId::from("other"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_composer")),
    )
    .expect("owner");
    assert!(!permit.matches(&foreign, &reference, Some(4)));
}

#[gpui_kit::test]
fn real_textarea_append_preserves_selection_and_native_draft(cx: &mut gpui_kit::TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("typed", window, cx);
                composer.set_selected_range(1..3, cx);
                assert!(composer.append_preserving_selection("\n\nqueued", window, cx));
                assert_eq!(composer.selected_range(), 1..3);
                assert_eq!(
                    composer.committed_snapshot().0.to_string(),
                    "typed\n\nqueued"
                );
            });
        });
    })
    .expect("test window remains available");
}

#[gpui_kit::test]
fn close_refuses_loading_or_failed_hydration_and_keeps_the_previous_durable_snapshot(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            workspace.composer_key = Some(owner.clone());
            workspace
                .composer
                .update(cx, |composer, cx| composer.set_value("new", window, cx));
            let durable = edit_reference(1, 7);
            let mut document = document("new");
            document.revision = 8;
            document.durable = Some(durable.clone());
            document.read = ReadState::Loading;
            workspace
                .composer_documents
                .documents
                .insert(owner.clone(), document);
            for read in [ReadState::Loading, ReadState::Failed] {
                workspace
                    .composer_documents
                    .documents
                    .get_mut(&owner)
                    .expect("retained owner")
                    .read = read;
                assert!(
                    workspace.freeze_composer_documents(cx).is_err(),
                    "partial hydration never grants a close snapshot"
                );
                assert!(workspace.composer_documents.frozen.is_none());
                let document = &workspace.composer_documents.documents[&owner];
                assert!(document.durable.as_ref() == Some(&durable));
                assert_eq!(
                    document.text.to_string(),
                    "new",
                    "typing stays available for local recovery"
                );
            }
        });
    })
    .expect("test window remains available");
}

#[gpui_kit::test]
fn canceled_ime_applies_a_deferred_companion_without_an_extra_keystroke(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::EntityInputHandler;

    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            workspace.composer_key = Some(owner.clone());
            workspace.drafts.save(owner.clone(), Draft::new("typed"));
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("typed", window, cx);
                composer.set_selected_range(5..5, cx);
                composer.replace_and_mark_text_in_range(None, "x", Some(1..1), window, cx);
            });
            let mut document = document("typed");
            document.read = ReadState::Loading;
            document.deferred = Some(Loaded {
                snapshot: None,
                headers: vec![(
                    key(),
                    "queued".into(),
                    SourceState::local(key(), &item("queued", "queued")),
                )],
                references: Vec::new(),
            });
            workspace
                .composer_documents
                .documents
                .insert(owner.clone(), document);
            workspace.apply_deferred_composer(&owner, window, cx);
            assert!(
                workspace.composer_documents.documents[&owner]
                    .deferred
                    .is_some()
            );
            assert!(
                !workspace.composer_documents_pending(),
                "IME-only waiting must not hold a readonly close barrier"
            );
            assert!(workspace.freeze_composer_documents(cx).is_err());
        });
    })
    .expect("test window remains available");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer.update(cx, |composer, cx| {
                composer.replace_and_mark_text_in_range(None, "", Some(0..0), window, cx);
            });
        });
    })
    .expect("test window remains available");
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .composer
                    .read(cx)
                    .committed_snapshot()
                    .0
                    .to_string(),
                "typed\n\nqueued"
            );
            let document = &workspace.composer_documents.documents[&owner()];
            assert!(document.deferred.is_none() && document.append.is_none());
            assert_eq!(
                document
                    .layout
                    .display(&document.text, &ComposerSourceKey::Native),
                "typed"
            );
            assert_eq!(
                workspace.drafts.get(&owner()).expect("native owner").text,
                "typed"
            );
            workspace.composer_documents.saves.clear();
        });
    })
    .expect("test window remains available");
}

#[gpui_kit::test]
fn reference_controls_do_not_mark_the_display_as_edited(cx: &mut gpui_kit::TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            let local = rich_item("#snippet", "captured expansion v1");
            let mut document = document("#snippet");
            document.layout.spans = vec![Span {
                kind: SpanKind::Source(key()),
                bytes: 8,
            }];
            document.layout.sources = vec![SourceState::local(key(), &local)];
            workspace
                .composer_documents
                .documents
                .insert(owner.clone(), document);
            for kind in [
                ComposerReferenceKind::File,
                ComposerReferenceKind::Agent,
                ComposerReferenceKind::Skill,
            ] {
                workspace
                    .remove_composer_reference(&owner, &key(), kind, 0, window, cx)
                    .expect("exact reference removal");
            }
            let state = &workspace.composer_documents.documents[&owner]
                .layout
                .sources[0];
            assert!(
                !state.edited,
                "only a text replacement may request snippet reprocessing"
            );
            let mut projected = local;
            assert!(
                !state
                    .project_display(&mut projected, "#snippet", true)
                    .expect("filtered source")
            );
            assert_eq!(projected.text, "captured expansion v1");
            workspace.composer_documents.saves.clear();
        });
    })
    .expect("test window remains available");
}

#[gpui_kit::test]
fn native_only_submission_captures_revision_authority_without_an_edit_document(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            workspace.composer_key = Some(owner.clone());
            workspace
                .composer
                .update(cx, |composer, cx| composer.set_value("typed", window, cx));
            let capture = workspace
                .capture_composer_submission(cx)
                .expect("ordinary native submission");
            assert!(capture.edit.is_none() && capture.snapshot.is_none());
            assert!(!workspace.composer_documents.contains(&owner));
            assert!(capture.generation > 0 && capture.revision > 0);
            assert!(
                capture
                    .native_authority
                    .as_ref()
                    .is_some_and(|authority| Arc::ptr_eq(
                        authority,
                        &workspace.composer_documents.native_authority
                    ))
            );
            let recovered = ComposerCapture::from_published_at(
                capture.composer.clone(),
                None,
                capture.generation,
                capture.revision,
            )
            .expect("published native metadata restores independently");
            assert!(
                recovered.native_authority.is_none(),
                "persisted counters grant no process-local consumption capability"
            );
            assert_eq!(
                workspace.composer_documents.native_revisions.get(&owner),
                Some(&capture.revision)
            );
            workspace
                .composer
                .update(cx, |composer, cx| composer.set_value("later", window, cx));
            workspace.save_composer_draft(cx);
            assert!(
                workspace.composer_documents.native_revisions[&owner] > capture.revision,
                "later native typing has its own consumption authority"
            );
        });
    })
    .expect("test window remains available");
}

#[gpui_kit::test]
fn autosave_completion_rearms_a_corrected_ready_revision_after_an_unwritten_failure(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            workspace.composer_key = Some(owner.clone());
            workspace.drafts.save(owner.clone(), Draft::default());
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("old", window, cx);
                composer.set_selected_range(3..3, cx);
            });
            let document = document("old");
            let captured = editing_capture(&document);
            let attempted = (captured.generation, captured.revision);
            workspace.composer_documents.documents.insert(owner.clone(), document);
            workspace.composer_documents.saves.insert(owner.clone(), Task::ready(()));
            let composer = workspace.composer.clone();
            composer.update(cx, |composer, cx| {
                composer.set_value("new", window, cx);
                composer.set_selected_range(3..3, cx);
            });
            workspace.on_committed_composer_edit(&composer, &event(0..3, "new", "new", 0, 1), window, cx);
            assert!(workspace.composer_documents.saves.contains_key(&owner), "the correction coalesces into the outstanding save");
            workspace.composer_documents.documents.get_mut(&owner).expect("corrected owner")
                .save_failed(captured, QueueCaptureFailure::Invalid);
            let document = &workspace.composer_documents.documents[&owner];
            assert!(document.dirty() && document.ready_for_capture(&[]));
            assert!(document.save_failure.is_none() && document.planned.is_none());
            workspace.finish_composer_autosave(&owner, Some(attempted), Err(QueueCaptureFailure::Invalid), window, cx);
            assert!(workspace.composer_documents.saves.contains_key(&owner),
                "the failed old task must re-arm the corrected ready revision without another edit or explicit Retry");
            workspace.composer_documents.saves.clear();
        });
    }).expect("the real autosave completion transition retains the corrected revision's debounce");
}

#[gpui_kit::test]
fn autosave_completion_does_not_retry_an_unchanged_failed_revision(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            let document = document("unchanged");
            assert!(document.dirty() && document.ready_for_capture(&[]));
            let attempted = (document.generation, document.revision);
            workspace.composer_documents.documents.insert(owner.clone(), document);
            workspace.composer_documents.saves.insert(owner.clone(), Task::ready(()));
            workspace.finish_composer_autosave(&owner, Some(attempted), Err(QueueCaptureFailure::Unavailable), window, cx);
            assert!(!workspace.composer_documents.saves.contains_key(&owner),
                "a same-revision failure cannot create a repeating autosave loop even without a retained failure");
        });
    }).expect("the completion owner refuses an unchanged failed retry");
}

#[gpui_kit::test]
fn autosave_completion_without_a_capture_does_not_infer_retry_authority(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            let mut document = document("current");
            document.generation = 7;
            document.revision = 99;
            assert!(document.dirty() && document.ready_for_capture(&[]));
            workspace
                .composer_documents
                .documents
                .insert(owner.clone(), document);
            workspace
                .composer_documents
                .saves
                .insert(owner.clone(), Task::ready(()));
            workspace.finish_composer_autosave(
                &owner,
                None,
                Err(QueueCaptureFailure::Unavailable),
                window,
                cx,
            );
            assert!(
                !workspace.composer_documents.saves.contains_key(&owner),
                "an unavailable capture supplies no baseline from which to infer newer input"
            );
        });
    })
    .expect("missing capture authority cannot cause automatic retries");
}

#[gpui_kit::test]
fn autosave_completion_keeps_newer_input_blocked_on_its_accepted_partial_put(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            let mut document = document("first");
            let plan = edit_reference(1, 1);
            document.planned = Some(plan.clone());
            let captured = editing_capture(&document);
            let attempted = (captured.generation, captured.revision);
            document
                .changed(&event(5..5, " later", "first later", 1, 2))
                .expect("newer input while an accepted put is pending");
            document.save_failed(captured, QueueCaptureFailure::Persistence);
            assert!(document.dirty() && !document.ready_for_capture(&[]));
            workspace
                .composer_documents
                .documents
                .insert(owner.clone(), document);
            workspace
                .composer_documents
                .saves
                .insert(owner.clone(), Task::ready(()));
            workspace.finish_composer_autosave(
                &owner,
                Some(attempted),
                Err(QueueCaptureFailure::Persistence),
                window,
                cx,
            );
            assert!(
                !workspace.composer_documents.saves.contains_key(&owner),
                "newer typing does not repair partial bytes automatically"
            );
            let failure = workspace.composer_documents.documents[&owner]
                .save_failure
                .as_ref()
                .expect("the immutable put still requires explicit Retry");
            assert!(
                failure.capture.planned.as_ref() == Some(&plan) && !failure.capture.explicit_retry
            );
        });
    })
    .expect("accepted partial puts retain explicit-only recovery through completion");
}

#[gpui_kit::test]
fn autosave_completion_rearms_a_newer_generation_than_the_captured_attempt(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let owner = owner();
            let mut document = document("independent current input");
            document.generation = 2;
            assert!(document.dirty() && document.ready_for_capture(&[]));
            workspace.composer_documents.documents.insert(owner.clone(), document);
            workspace.composer_documents.saves.insert(owner.clone(), Task::ready(()));
            workspace.finish_composer_autosave(&owner, Some((1, 50)), Err(QueueCaptureFailure::Unavailable), window, cx);
            assert!(workspace.composer_documents.saves.contains_key(&owner),
                "a newer generation can save even when its revision restarts below the completed attempt's revision");
            workspace.composer_documents.saves.clear();
        });
    }).expect("autosave completion compares generation before revision");
}
