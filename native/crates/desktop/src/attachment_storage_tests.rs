//! Deterministic storage transitions over private files, a real journal worker and headless GPUI.
//! Test gates hold named coordinator boundaries with one-shot receipts, without timer races.

use std::fs::{self, OpenOptions};
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext, WindowOptions};
use openchamber_native_core::draft::DraftTarget;
use openchamber_native_core::ids::{DraftId, RuntimeId};
use openchamber_native_core::synthetic::WorkloadSize;

use crate::locale::Strings;
use crate::mutation_journal::{
    JournalCommit, JournalSnapshot, LoadedJournal, MAX_TEXT, MutationJournal,
};
use crate::theme::{self, Appearance, Palette};
use crate::workspace::Source;

use super::*;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

fn owner(name: &str) -> DraftKey {
    DraftKey::new(
        RuntimeId::from("captured-runtime"),
        "/captured/project",
        DraftTarget::NewSession(DraftId::from(name)),
    )
    .expect("valid synthetic owner")
}
fn input(name: &str, bytes: Vec<u8>) -> AttachmentIntake {
    AttachmentIntake {
        source: AttachmentInputSource::Bytes(bytes),
        filename: name.to_owned(),
        declared_media_type: None,
        reserved_filenames: Vec::new(),
    }
}
fn path_input(name: &str) -> AttachmentIntake {
    AttachmentIntake {
        source: AttachmentInputSource::LocalPath(PathBuf::from("/private/selected-file")),
        ..input(name, Vec::new())
    }
}
fn file(name: &str, mime: &'static str, bytes: &[u8]) -> NormalizedAttachmentFile {
    NormalizedAttachmentFile {
        filename: name.to_owned(),
        media_type: mime,
        content: bytes.to_vec().into_boxed_slice().into_vec(),
    }
}
fn document() -> Vec<NormalizedAttachmentFile> {
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("synthetic embedded PNG");
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("content.xml", br#"<office:document><text:p>Document</text:p><draw:image xlink:href="Pictures/image.png"/></office:document>"#.as_slice()),
        ("Pictures/image.png", png.get_ref().as_slice()),
    ] {
        archive.start_file(name, zip::write::SimpleFileOptions::default()).expect("synthetic document part");
        archive.write_all(bytes).expect("complete document part");
    }
    let bytes = archive
        .finish()
        .expect("complete document archive")
        .into_inner();
    let files = attachment_intake::prepare_attachment(input("source.odt", bytes))
        .expect("actual Office/OpenDocument intake emits text and its cited image");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].media_type, "text/plain");
    assert_eq!(files[1].media_type, "image/png");
    files
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/opencode/attachment-storage-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).expect("unique private fixture");
        Self(path)
    }
    fn journal(&self) -> PathBuf {
        self.0.join("profile/journal.json")
    }
    fn blob(&self, id: &AttachmentId) -> PathBuf {
        self.0
            .join("profile/attachment-spool")
            .join(format!("blob-{}.bin", id.as_str()))
    }
    fn collision(&self, id: &AttachmentId) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(self.blob(id))
            .expect("exact owned collision fixture")
            .write_all(b"x")
            .expect("one-byte collision");
    }
    fn validator(&self) -> MutationJournal {
        MutationJournal::open(&self.0.join("validation.json"))
            .expect("separate canonical validator journal")
            .0
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open(cx: &mut TestAppContext, fixture: &Fixture) -> (AnyWindowHandle, Entity<Workspace>) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(Strings::load("en"));
        theme::apply(Palette::default_for(Appearance::Dark), cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            window.set_layout_mode(gpui_kit::LayoutMode::Logical);
            cx.new(|cx| {
                let mut workspace =
                    Workspace::new(Source::Synthetic(WorkloadSize::Small), window, cx);
                workspace.project_readouts.without_app_icon();
                // The supplied owner intentionally differs from the synthetic runtime. Local
                // publication is permitted; staging can never start a network request here.
                workspace.composer_key = None;
                workspace.open_persistence(fixture.journal(), window, cx);
                workspace
            })
        })
        .expect("headless window opens")
    })
}

fn seed_source(
    workspace: &mut Workspace,
    owner: DraftKey,
    files: Vec<NormalizedAttachmentFile>,
) -> AttachmentId {
    let ticket = workspace
        .attachment_storage
        .next_sequence()
        .expect("input generation");
    workspace.attachment_storage.rows.insert(
        ticket,
        AttachmentStorageRow {
            ticket,
            owner: owner.clone(),
            filename: files[0].filename.clone(),
            id: None,
            source_document_id: None,
            status: AttachmentStorageStatus::Preparing,
        },
    );
    workspace.attachment_storage.active = Some(ActiveSource {
        ticket,
        owner: owner.clone(),
        key: None,
        group: None,
        cancelled: false,
    });
    let source = workspace
        .capture_normalized_source(ticket, owner, files)
        .expect("complete source admission");
    let key = source.key.clone();
    workspace
        .attachment_storage
        .fail(ticket, AttachmentStorageFailure::Persistence);
    workspace
        .attachment_storage
        .sources
        .insert(key.clone(), source);
    workspace.attachment_storage.active = None;
    key
}

fn activate_for_publication(workspace: &mut Workspace, key: &AttachmentId) -> CapturedSource {
    let mut source = workspace
        .attachment_storage
        .sources
        .remove(key)
        .expect("captured source");
    for member in &mut source.members {
        member.descriptor = Some(BlobDescriptor::new(
            member.hold.blob,
            member.file.content.len() as u64,
            [7; 32],
        ));
    }
    workspace.attachment_storage.active = Some(ActiveSource {
        ticket: source.ticket,
        owner: source.owner.clone(),
        key: Some(source.key.clone()),
        group: source.group.clone(),
        cancelled: false,
    });
    source
}

fn gate() -> (
    TestGate,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached, arrived) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    (
        TestGate {
            reached,
            release: resume,
        },
        arrived,
        release,
    )
}

async fn finish_worker(workspace: &Entity<Workspace>, cx: &mut TestAppContext) {
    let receipt = workspace.update(cx, |workspace, _| {
        workspace.attachment_worker().expect("real worker").finish()
    });
    assert!(
        matches!(receipt.await, Ok(Ok(CommitOutcome::Durable))),
        "real worker drains and releases its lock"
    );
    cx.run_until_parked();
}

// These snapshots are test-owned copies for comparison with the actual persisted format.
// Production publication borrows the parent's canonical preflight and moves DraftContent.
fn snapshot(workspace: &Workspace) -> JournalSnapshot {
    JournalSnapshot {
        drafts: workspace
            .drafts
            .iter()
            .map(|(owner, draft)| (owner.clone(), draft.clone()))
            .collect(),
        attempts: workspace.send_attempts.snapshot(),
        creations: workspace.creations.snapshot(),
        selections: workspace.composer_selections.snapshot(),
        attachments: workspace.attachments.clone(),
        preparations: workspace.attachment_preparations.clone(),
        retired_blobs: workspace.retired_attachment_blobs.clone(),
        starter_captures: workspace.starters.captures.snapshot(),
        queue: workspace.queue_storage.clone(),
    }
}
fn candidate(workspace: &Workspace, source: &CapturedSource) -> JournalSnapshot {
    let mut snapshot = snapshot(workspace);
    let mut draft = workspace
        .drafts
        .get(&source.owner)
        .cloned()
        .unwrap_or_default();
    for member in &source.members {
        draft.content.files.push(member.metadata(&source.group));
        snapshot.attachments.insert(
            member.hold.id.clone(),
            PreparedAttachment {
                blob: member.descriptor.expect("receipt fixture"),
                staging: None,
            },
        );
    }
    snapshot.drafts.retain(|(owner, _)| owner != &source.owner);
    snapshot.drafts.push((source.owner.clone(), draft));
    snapshot
        .preparations
        .retain(|hold| !source.members.iter().any(|member| &member.hold == hold));
    snapshot
}
fn save_valid(journal: &mut MutationJournal, snapshot: &JournalSnapshot) {
    assert!(
        matches!(journal.save(snapshot), Ok(JournalCommit::Durable)),
        "actual complete snapshot remains valid"
    );
}

#[gpui_kit::test]
async fn selected_native_path_is_withheld_without_a_native_owned_local_server(
    cx: &mut TestAppContext,
) {
    // Electron publishes a picked path only to its own local instance. Native
    // connects to externally owned direct hosts only, loopback ones included.
    let fixture = Fixture::new();
    let directory = fixture.0.join("selected λ");
    fs::create_dir(&directory).expect("test-owned selected-file directory");
    let selected = directory.join("notes.txt");
    fs::write(&selected, b"original").expect("complete test-owned selected file");
    let selected_text = selected
        .to_str()
        .expect("literal Unicode selected path")
        .to_owned();
    let captured_owner = owner("native-path");
    let sibling_owner = owner("independent-path");
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
        .await;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.drafts.save(
                captured_owner.clone(),
                openchamber_native_core::draft::Draft::new("keep current input"),
            );
            workspace.drafts.save(
                sibling_owner.clone(),
                openchamber_native_core::draft::Draft::new("independent input"),
            );
            workspace.attach_selected_paths(
                captured_owner.clone(),
                vec![selected.clone()],
                window,
                cx,
            );
        })
    })
    .expect("the actual picker/drop path adapter owns this explicit local selection");
    cx.condition(&workspace, |workspace, _| {
        !workspace.attachment_storage.is_busy()
            && !workspace.attachment_storage.has_pending(&captured_owner)
            && workspace
                .drafts
                .get(&captured_owner)
                .is_some_and(|draft| draft.content.files.len() == 1)
    })
    .await;
    let (id, descriptor) = workspace.read_with(cx, |workspace, _| {
        let draft = workspace
            .drafts
            .get(&captured_owner)
            .expect("actual intake publication");
        assert_eq!(draft.text, "keep current input");
        let file = &draft.content.files[0];
        assert_eq!(
            file.name.as_deref(),
            Some("notes.txt"),
            "the UI and upload filename remains a basename"
        );
        assert_eq!(file.media_type.as_deref(), Some("text/plain"));
        assert_eq!(file.byte_len, Some(8));
        assert!(matches!(file.content, DraftFileContent::Prepared));
        assert!(file.source_document_id.is_none());
        let prepared = workspace
            .attachments
            .get(&file.id)
            .expect("actual durable prepared bytes");
        assert!(
            prepared.staging.is_none(),
            "intake alone does not stage bytes or grant a remote target"
        );
        assert_eq!(
            prepared.blob.sha256(),
            &crate::queue_capture::sha256(b"original")
        );
        (file.id.clone(), prepared.blob)
    });
    finish_worker(&workspace, cx).await;
    cx.update_window(window, |_, window, _| window.remove_window())
        .expect("close only this fixture window");
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.journal()).expect("the actual durable journal"))
            .expect("actual stored capture JSON");
    let stored = raw["drafts"]
        .as_array()
        .expect("stored owner-scoped drafts")
        .iter()
        .flat_map(|draft| {
            draft["content"]["owned"]["files"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .find(|file| file["id"].as_str() == Some(id.as_str()))
        .expect("the published selected file is durable");
    assert_eq!(stored["name"], "notes.txt");
    assert!(
        stored.get("agent_path").is_none(),
        "an external host never receives the picked path"
    );
    assert!(
        !raw.to_string().contains(&selected_text),
        "the journal retains no copy of the picked path"
    );
    let (_journal, loaded) = MutationJournal::open(&fixture.journal())
        .expect("restart uses the canonical journal reader");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("selected source remains durably owned");
    };
    let draft = &restored
        .drafts
        .iter()
        .find(|(key, _)| key == &captured_owner)
        .expect("same captured owner")
        .1;
    let encoded = serde_json::to_value(crate::draft_storage::StoredDraft::capture(
        &captured_owner,
        draft,
    ))
    .expect("canonical restored semantic metadata");
    assert!(
        encoded["content"]["owned"]["files"][0]
            .get("agent_path")
            .is_none()
    );
    assert_eq!(draft.text, "keep current input");
    assert_eq!(
        restored
            .drafts
            .iter()
            .find(|(key, _)| key == &sibling_owner)
            .expect("independent owner survives")
            .1
            .text,
        "independent input"
    );
    assert_eq!(
        restored
            .attachments
            .get(&id)
            .expect("same catalog custody after restart")
            .blob,
        descriptor
    );
    assert_eq!(
        fs::read(fixture.blob(&id)).expect("exact retained prepared bytes"),
        b"original"
    );
    assert_eq!(
        fs::read(&selected).expect("the selected original remains untouched"),
        b"original"
    );
    assert!(restored.preparations.is_empty() && restored.retired_blobs.is_empty());
}

#[gpui_kit::test]
async fn sanitized_native_sources_and_memory_inputs_never_publish_original_paths(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let har = fixture.0.join("capture.hAr");
    let notebook = fixture.0.join("Notebook.IPYNB");
    let har_bytes = br#"{"log":{"entries":[{"request":{"url":"https://example.invalid/?token=fixture-token","headers":[{"name":"Authorization","value":"fixture-credential"}],"postData":{"text":"fixture-request-body"}},"response":{"content":{"text":"fixture-response-body"}}}]}}"#;
    let notebook_bytes = br#"{"cells":[{"cell_type":"code","source":["print(1)"],"outputs":[{"output_type":"stream","text":["fixture-notebook-output"]}]}]}"#;
    fs::write(&har, har_bytes).expect("test-owned raw HAR");
    fs::write(&notebook, notebook_bytes).expect("test-owned raw notebook");
    let captured_owner = owner("sanitized-paths");
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
        .await;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.attach_selected_paths(
                captured_owner.clone(),
                vec![har.clone(), notebook.clone()],
                window,
                cx,
            );
            assert!(
                workspace
                    .ingest_attachments(
                        captured_owner.clone(),
                        vec![input("memory.txt", b"memory payload".to_vec())],
                        window,
                        cx
                    )
                    .is_empty()
            );
        })
    })
    .expect("ordinary selected-path and memory intake share their captured owner");
    cx.condition(&workspace, |workspace, _| {
        !workspace.attachment_storage.is_busy()
            && !workspace.attachment_storage.has_pending(&captured_owner)
            && workspace
                .drafts
                .get(&captured_owner)
                .is_some_and(|draft| draft.content.files.len() == 3)
    })
    .await;
    workspace.read_with(cx, |workspace, _| {
        let draft = workspace
            .drafts
            .get(&captured_owner)
            .expect("all independent normalized sources publish");
        let stored = serde_json::to_value(crate::draft_storage::StoredDraft::capture(
            &captured_owner,
            draft,
        ))
        .expect("canonical per-use metadata");
        for file in stored["content"]["owned"]["files"]
            .as_array()
            .expect("all normalized file entries")
        {
            assert!(
                file.get("agent_path").is_none(),
                "sanitized and memory inputs cannot advertise a path that bypasses prepared bytes"
            );
        }
        for file in &draft.content.files {
            assert!(matches!(file.content, DraftFileContent::Prepared));
            let bytes = fs::read(fixture.blob(&file.id)).expect("actual normalized spool bytes");
            match file.name.as_deref().expect("normalized basename") {
                "capture.hAr" => {
                    let text = std::str::from_utf8(&bytes).expect("sanitized HAR text");
                    for original in [
                        "fixture-token",
                        "fixture-credential",
                        "fixture-request-body",
                        "fixture-response-body",
                    ] {
                        assert!(
                            !text.contains(original),
                            "selected HAR cannot retain redacted source values"
                        );
                    }
                }
                "Notebook.IPYNB" => {
                    let text = std::str::from_utf8(&bytes).expect("prepared notebook text");
                    assert!(text.contains("print(1)"));
                    // As upstream's prepareNotebook, text outputs stay in the prepared bytes.
                    assert!(
                        text.contains("### Output") && text.contains("fixture-notebook-output")
                    );
                }
                "memory.txt" => assert_eq!(bytes, b"memory payload"),
                _ => panic!("intake changed an independent normalized filename"),
            }
        }
    });
    finish_worker(&workspace, cx).await;
    cx.update_window(window, |_, window, _| window.remove_window())
        .expect("close only the sanitized-source test window");
    let persisted = fs::read_to_string(fixture.journal()).expect("actual saved source metadata");
    assert!(
        !persisted.contains(har.to_str().expect("test path"))
            && !persisted.contains(notebook.to_str().expect("test path"))
    );
    let (_journal, loaded) = MutationJournal::open(&fixture.journal())
        .expect("canonical restart accepts the sanitized metadata");
    let LoadedJournal::Present(restored) = loaded else {
        panic!("all source byte roots remain durable");
    };
    assert_eq!(
        restored
            .drafts
            .iter()
            .find(|(key, _)| key == &captured_owner)
            .expect("same captured owner")
            .1
            .content
            .files
            .len(),
        3
    );
    assert_eq!(
        fs::read(&har).expect("raw selected HAR remains untouched"),
        har_bytes
    );
    assert_eq!(
        fs::read(&notebook).expect("raw selected notebook remains untouched"),
        notebook_bytes
    );
}

#[test]
fn queue_refusals_return_original_buffers_and_do_not_remove_accepted_siblings() {
    let mut storage = AttachmentStorage::default();
    storage
        .admit(
            owner("a"),
            input("a.txt", vec![1; MAX_INLINE_ATTACHMENT_BYTES]),
        )
        .expect("first input");
    storage
        .admit(
            owner("b"),
            input("b.txt", vec![2; MAX_INLINE_ATTACHMENT_BYTES]),
        )
        .expect("second input");
    let mut bytes = Vec::with_capacity(1024);
    bytes.extend_from_slice(b"unchanged");
    let pointer = bytes.as_ptr();
    let rejected = storage
        .admit(owner("c"), input("c.txt", bytes))
        .expect_err("queue capacity is inclusive");
    let AttachmentInputSource::Bytes(bytes) = rejected.input.source else {
        panic!("original memory source")
    };
    assert_eq!(bytes.as_ptr(), pointer);
    assert_eq!(bytes, b"unchanged");
    assert_eq!(storage.queue.len(), 2);
    assert_eq!(storage.queued_bytes, MAX_QUEUED_BYTES);
}

#[test]
fn clipboard_allocations_share_the_queue_capacity_and_metadata_limits() {
    let mut storage = AttachmentStorage::default();
    let clipboard = AttachmentIntake {
        source: AttachmentInputSource::ClipboardImage {
            bytes: Vec::with_capacity(MAX_QUEUED_BYTES),
            format: gpui_kit::ImageFormat::Bmp,
        },
        ..input("clipboard.bmp", Vec::new())
    };
    assert!(bounded_metadata(&clipboard));
    storage
        .admit(owner("a"), clipboard)
        .expect("exact clipboard allocation bound");
    assert_eq!(storage.queued_bytes, MAX_QUEUED_BYTES);
    assert!(
        storage
            .admit(owner("b"), input("extra.txt", vec![0]))
            .is_err()
    );
    let mut empty = AttachmentStorage::default();
    let oversized = AttachmentIntake {
        source: AttachmentInputSource::ClipboardImage {
            bytes: vec![0; MAX_INLINE_ATTACHMENT_BYTES + 1],
            format: gpui_kit::ImageFormat::Bmp,
        },
        ..input("oversized.bmp", Vec::new())
    };
    empty
        .admit(owner("a"), oversized)
        .expect_err("whole raw input exceeds admission");
    assert!(empty.queue.is_empty());
}

#[test]
fn individual_ticket_cancel_releases_only_its_queued_memory_and_metadata() {
    let mut storage = AttachmentStorage::default();
    storage
        .admit(owner("same"), input("first.txt", vec![1; 8]))
        .expect("first input");
    storage
        .admit(owner("same"), input("second.txt", vec![2; 16]))
        .expect("second input");
    storage.cancel_ticket(1);
    assert_eq!(storage.queue.len(), 1);
    assert_eq!(storage.queued_bytes, 16);
    assert_eq!(storage.queued_metadata, 1);
    assert!(!storage.rows.contains_key(&1));
    assert_eq!(storage.rows[&2].filename, "second.txt");
    storage.active = Some(ActiveSource {
        ticket: 2,
        owner: owner("same"),
        key: None,
        group: None,
        cancelled: false,
    });
    storage.cancel_ticket(2);
    assert!(
        !storage.current_input(2),
        "an active background intake cannot publish after ticket discard"
    );
}

#[test]
fn metadata_bounds_and_debug_are_content_free() {
    let mut storage = AttachmentStorage::default();
    for _ in 0..MAX_ENTRIES {
        storage
            .admit(owner("private-owner"), path_input("private-filename.txt"))
            .expect("bounded path metadata");
    }
    assert!(
        storage
            .admit(owner("a"), path_input("overflow.txt"))
            .is_err()
    );
    assert_eq!(storage.queued_metadata, MAX_ENTRIES);
    assert!(!format!("{storage:?}").contains("private"));
    assert!(!format!("{:?}", storage.rows.values().next().expect("row")).contains("private"));
    let id = random_label(1);
    assert_eq!(id.len(), 32);
    assert!(BlobId::parse(&id).is_ok());
    storage.sequence = u64::MAX;
    assert_eq!(
        storage.next_sequence(),
        Err(AttachmentStorageFailure::IdentityExhausted)
    );
}

#[gpui_kit::test]
async fn queued_retry_discard_is_recorded_before_the_coordinator_runs(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (discarded, sibling, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let discarded = seed_source(workspace, owner("same"), document());
                let sibling = seed_source(
                    workspace,
                    owner("same"),
                    vec![file("sibling.txt", "text/plain", b"sibling")],
                );
                workspace.retry_attachment_storage(&discarded, window, cx);
                workspace.discard_attachment_capture(&discarded, window, cx);
                assert!(
                    workspace.attachment_storage.sources[&discarded].cancelled,
                    "queued retry is revoked synchronously"
                );
                workspace.retry_attachment_storage(&sibling, window, cx);
                (
                    discarded,
                    sibling,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("accepted coordinator"),
                )
            })
        })
        .expect("headless window");
    drain.await;
    workspace.update(cx, |workspace, _| {
        let files = &workspace
            .drafts
            .get(&owner("same"))
            .expect("sibling draft")
            .content
            .files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, sibling);
        assert!(!workspace.attachments.contains_key(&discarded));
        assert!(!workspace.has_uncommitted_attachment_storage());
        assert_eq!(workspace.attachment_storage.recovery_bytes, 0);
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn ticket_cancel_retires_an_idle_capture_and_preserves_the_next_source(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (sibling, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let discarded = seed_source(workspace, owner("same"), document());
                let sibling = seed_source(
                    workspace,
                    owner("same"),
                    vec![file("kept.txt", "text/plain", b"kept")],
                );
                let ticket = workspace.attachment_storage.sources[&discarded].ticket;
                workspace.cancel_attachment_input(ticket, window, cx);
                assert!(
                    workspace.attachment_storage.sources[&discarded].cancelled,
                    "an idle capture loses publication authority synchronously"
                );
                workspace.retry_attachment_storage(&sibling, window, cx);
                (
                    sibling,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("ticket cleanup coordinator"),
                )
            })
        })
        .expect("window");
    drain.await;
    workspace.update(cx, |workspace, _| {
        let files = &workspace
            .drafts
            .get(&owner("same"))
            .expect("independent sibling")
            .content
            .files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, sibling);
        assert!(!workspace.has_uncommitted_attachment_storage());
        assert_eq!(workspace.attachment_storage.recovery_bytes, 0);
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn individual_preparing_input_cancel_preserves_the_same_owners_next_source(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (prepared_gate, arrived, release) = gate();
    let (ticket, next_ticket, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.attachment_storage.gates.prepared = Some(prepared_gate);
                let rejected = workspace.ingest_attachments(
                    owner("same"),
                    vec![
                        input("discarded.txt", b"discarded".to_vec()),
                        input("kept.txt", b"kept".to_vec()),
                    ],
                    window,
                    cx,
                );
                assert!(rejected.is_empty());
                let tickets: Vec<_> = workspace
                    .attachment_storage
                    .rows(&owner("same"))
                    .map(|row| row.ticket)
                    .collect();
                (
                    tickets[0],
                    tickets[1],
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("accepted input coordinator"),
                )
            })
        })
        .expect("window");
    arrived
        .await
        .expect("actual background normalization completed before identity assignment");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.attachment_storage.rows[&ticket].id.is_none());
            workspace.cancel_attachment_input(ticket, window, cx);
            assert!(
                workspace
                    .attachment_storage
                    .active
                    .as_ref()
                    .expect("active normalization owner")
                    .cancelled
            );
            assert!(workspace.attachment_storage.rows.contains_key(&next_ticket));
            assert!(
                workspace.attachment_preparations.is_empty(),
                "discarded normalization has no durable side effects"
            );
        })
    })
    .expect("window");
    release
        .send(())
        .expect("resume normalized-source ownership");
    drain.await;
    workspace.update(cx, |workspace, _| {
        let files = &workspace
            .drafts
            .get(&owner("same"))
            .expect("unrelated source is retained")
            .content
            .files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name.as_deref(), Some("kept.txt"));
        assert_eq!(workspace.attachment_storage.queued_bytes, 0);
        assert_eq!(workspace.attachment_storage.queued_metadata, 0);
        assert!(!workspace.has_uncommitted_attachment_storage());
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn active_discard_drains_the_real_put_receipt_before_exact_source_retirement(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (held, arrived, release) = gate();
    let (discarded, sibling, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.attachment_storage.gates.put_accepted = Some(held);
                let discarded = seed_source(workspace, owner("same"), document());
                let sibling = seed_source(
                    workspace,
                    owner("same"),
                    vec![file("next.txt", "text/plain", b"next")],
                );
                workspace.retry_attachment_storage(&discarded, window, cx);
                workspace.retry_attachment_storage(&sibling, window, cx);
                (
                    discarded,
                    sibling,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("coordinator"),
                )
            })
        })
        .expect("window");
    arrived.await.expect("first real put was accepted");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.discard_attachment_capture(&discarded, window, cx);
            assert!(
                workspace
                    .attachment_storage
                    .active
                    .as_ref()
                    .expect("receipt owner")
                    .cancelled
            );
            assert!(
                workspace.attachment_storage.is_busy(),
                "discard keeps the accepted receipt future"
            );
            assert!(
                workspace.attachment_storage.recovery_bytes > 0,
                "the source is retained until its receipt settles"
            );
            assert!(
                workspace.drafts.get(&owner("same")).is_none(),
                "no partial source is visible"
            );
        })
    })
    .expect("window");
    release.send(()).expect("resume exact receipt owner");
    drain.await;
    workspace.update(cx, |workspace, _| {
        assert_eq!(
            workspace
                .drafts
                .get(&owner("same"))
                .expect("next source")
                .content
                .files[0]
                .id,
            sibling
        );
        assert!(!workspace.attachments.contains_key(&discarded));
        assert!(!workspace.has_uncommitted_attachment_storage());
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn later_image_put_failure_keeps_the_whole_source_private_and_retry_reuses_ids(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (before, failed, after, image, ids, original_text, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let before = seed_source(
                    workspace,
                    owner("same"),
                    vec![file("before.txt", "text/plain", b"before")],
                );
                let failed = seed_source(workspace, owner("same"), document());
                let after = seed_source(
                    workspace,
                    owner("same"),
                    vec![file("after.txt", "text/plain", b"after")],
                );
                let ids: Vec<_> = workspace.attachment_storage.sources[&failed]
                    .members
                    .iter()
                    .map(|member| member.hold.id.clone())
                    .collect();
                let original_text = workspace.attachment_storage.sources[&failed].members[0]
                    .file
                    .content
                    .clone();
                let image = ids[1].clone();
                fixture.collision(&image);
                for key in [&before, &failed, &after] {
                    workspace.retry_attachment_storage(key, window, cx);
                }
                (
                    before,
                    failed,
                    after,
                    image,
                    ids,
                    original_text,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("coordinator"),
                )
            })
        })
        .expect("window");
    drain.await;
    workspace.update(cx, |workspace, _| {
        let files = &workspace
            .drafts
            .get(&owner("same"))
            .expect("independent successes")
            .content
            .files;
        assert_eq!(
            files.iter().map(|file| &file.id).collect::<Vec<_>>(),
            vec![&before, &after]
        );
        let source = &workspace.attachment_storage.sources[&failed];
        assert_eq!(
            source.phase,
            SourcePhase::Proposed,
            "text remains private when its image fails"
        );
        assert!(
            source.members[0].descriptor.is_some(),
            "successful private put is retained"
        );
        assert!(source.members[1].descriptor.is_none());
        assert!(
            source
                .members
                .iter()
                .all(|member| !workspace.attachments.contains_key(&member.hold.id))
        );
        assert_eq!(workspace.attachment_preparations.len(), 2);
        assert!(
            !workspace.attachment_storage.has_pending(&owner("same")),
            "a failed source does not hold unrelated intake"
        );
    });
    assert_eq!(
        fs::read(fixture.blob(&failed)).expect("private successful text blob"),
        original_text
    );
    fs::remove_file(fixture.blob(&image)).expect("correct only the owned failure fixture");
    let drain = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.retry_attachment_storage(&failed, window, cx);
                workspace
                    .attachment_storage
                    .take_drain()
                    .expect("explicit source retry")
            })
        })
        .expect("window");
    drain.await;
    workspace.update(cx, |workspace, _| {
        let files = &workspace
            .drafts
            .get(&owner("same"))
            .expect("complete source")
            .content
            .files;
        assert!(
            ids.iter().all(|id| files.iter().any(|file| &file.id == id)),
            "retry mints no replacement identities"
        );
        assert_eq!(
            files.len(),
            4,
            "both source members publish together after independent siblings"
        );
        assert!(
            files
                .iter()
                .filter(|file| ids.contains(&file.id))
                .all(|file| file.source_document_id == files[2].source_document_id)
        );
        assert!(!workspace.has_uncommitted_attachment_storage());
    });
    finish_worker(&workspace, cx).await;
    let (_journal, loaded) = MutationJournal::open(&fixture.journal())
        .expect("restart reads the actual durable journal");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("persisted complete source")
    };
    assert!(snapshot.preparations.is_empty());
    let draft = &snapshot
        .drafts
        .iter()
        .find(|(key, _)| key == &owner("same"))
        .expect("source owner survives restart")
        .1;
    assert_eq!(draft.content.files.len(), 4);
    assert!(
        ids.iter()
            .all(|id| draft.content.files.iter().any(|file| &file.id == id))
    );
}

#[gpui_kit::test]
async fn failed_document_restart_has_only_unfinished_holds_and_no_partial_draft_projection(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (ids, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let key = seed_source(workspace, owner("document"), document());
                let ids: Vec<_> = workspace.attachment_storage.sources[&key]
                    .members
                    .iter()
                    .map(|member| member.hold.id.clone())
                    .collect();
                fixture.collision(&ids[1]);
                workspace.retry_attachment_storage(&key, window, cx);
                (
                    ids,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("source coordinator"),
                )
            })
        })
        .expect("window");
    drain.await;
    finish_worker(&workspace, cx).await;
    let (journal, loaded) =
        MutationJournal::open(&fixture.journal()).expect("canonical restart load");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("unfinished holds are durable")
    };
    assert_eq!(snapshot.preparations.len(), 2);
    assert!(
        snapshot
            .preparations
            .iter()
            .all(|hold| ids.contains(&hold.id))
    );
    assert!(snapshot.drafts.iter().all(|(_, draft)| {
        draft
            .content
            .files
            .iter()
            .all(|file| !ids.contains(&file.id))
    }));
    assert!(snapshot.attachments.is_empty());
    drop(journal);
    let (_new_window, restarted) = open(cx, &fixture);
    cx.condition(&restarted, |workspace, _| workspace.persistence_loaded())
        .await;
    restarted.update(cx, |workspace, _| {
        assert_eq!(workspace.attachment_preparations.len(), 2);
        assert!(
            workspace.attachment_storage.sources.is_empty(),
            "startup never reconstructs unknown source metadata or replays I/O"
        );
        assert!(!workspace.attachment_storage.is_busy());
        assert!(
            workspace.drafts.get(&owner("document")).is_none(),
            "send projection has no dangling citation file"
        );
    });
    finish_worker(&restarted, cx).await;
}

#[gpui_kit::test]
async fn cancelled_published_source_cleans_failed_barrier_capture_and_preserves_other_live_roots(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (published_gate, published, publish_resume) = gate();
    let (submitted_gate, submitted, submit_resume) = gate();
    let (key, group, ids, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.attachment_storage.gates.published = Some(published_gate);
                workspace.attachment_storage.gates.publication_submitted = Some(submitted_gate);
                let key = seed_source(workspace, owner("document"), document());
                let source = &workspace.attachment_storage.sources[&key];
                let group = source.group.clone().expect("actual multiple-output group");
                let ids = source
                    .members
                    .iter()
                    .map(|member| member.hold.id.clone())
                    .collect::<Vec<_>>();
                workspace.retry_attachment_storage(&key, window, cx);
                (
                    key,
                    group,
                    ids,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("coordinator"),
                )
            })
        })
        .expect("window");
    published
        .await
        .expect("all source puts completed and atomic memory publication occurred");
    finish_worker(&workspace, cx).await; // The next actual journal receipt will be Closed.
    publish_resume
        .send(())
        .expect("allow submission to the closed real worker");
    submitted
        .await
        .expect("publication receipt is owned but not yet consumed");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let image = workspace
                .drafts
                .get(&owner("document"))
                .expect("whole source exists")
                .content
                .files[1]
                .clone();
            let mut other = Draft::new("another live root");
            other.content.files.push(image);
            workspace.drafts.save(owner("other"), other);
            workspace.cancel_attachment_group(&owner("document"), &group, window, cx);
            workspace.drafts.take(&owner("document"));
        })
    })
    .expect("window");
    submit_resume
        .send(())
        .expect("consume the failed publication receipt");
    drain.await;
    workspace.update(cx, |workspace, _| {
        assert!(!workspace.attachment_storage.sources.contains_key(&key));
        assert!(
            workspace
                .attachment_storage
                .rows(&owner("document"))
                .next()
                .is_none()
        );
        assert_eq!(workspace.attachment_storage.recovery_bytes, 0);
        assert!(
            !workspace.has_uncommitted_attachment_storage(),
            "no cancelled Published capture blocks normal close"
        );
        assert!(
            !workspace.attachments.contains_key(&ids[0]),
            "the coordinator admits unowned text retirement after the failed barrier"
        );
        assert!(
            workspace
                .retired_attachment_blobs
                .iter()
                .any(|blob| blob.as_str() == ids[0].as_str()),
            "a failed worker keeps the named retirement intent"
        );
        assert!(
            workspace.attachments.contains_key(&ids[1]),
            "another live root protects the image catalog descriptor"
        );
        assert!(
            workspace
                .drafts
                .get(&owner("other"))
                .expect("other root")
                .content
                .files
                .iter()
                .any(|file| file.id == ids[1])
        );
        assert!(
            !workspace
                .retired_attachment_blobs
                .iter()
                .any(|blob| blob.as_str() == ids[1].as_str())
        );
    });
    assert!(
        fixture.blob(&ids[1]).is_file(),
        "shared image bytes are never physically released"
    );
    cx.run_until_parked();
}

#[gpui_kit::test]
async fn published_owner_removal_without_a_group_cleans_a_failed_publication(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (published_gate, published, publish_resume) = gate();
    let (submitted_gate, submitted, submit_resume) = gate();
    let (key, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.attachment_storage.gates.published = Some(published_gate);
                workspace.attachment_storage.gates.publication_submitted = Some(submitted_gate);
                let key = seed_source(
                    workspace,
                    owner("single"),
                    vec![file("single.txt", "text/plain", b"complete")],
                );
                workspace.retry_attachment_storage(&key, window, cx);
                (
                    key,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("single source coordinator"),
                )
            })
        })
        .expect("window");
    published
        .await
        .expect("single source published after its put");
    finish_worker(&workspace, cx).await;
    publish_resume
        .send(())
        .expect("submit publication to the closed worker");
    submitted.await.expect("publication barrier owned");
    workspace.update(cx, |workspace, _| {
        workspace.drafts.take(&owner("single"));
    });
    submit_resume
        .send(())
        .expect("observe failed barrier after owner removal");
    drain.await;
    workspace.update(cx, |workspace, _| {
        assert!(!workspace.attachments.contains_key(&key));
        assert!(!workspace.has_uncommitted_attachment_storage());
        assert_eq!(workspace.attachment_storage.recovery_bytes, 0);
        assert!(
            workspace
                .retired_attachment_blobs
                .iter()
                .any(|blob| blob.as_str() == key.as_str())
        );
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
async fn published_discard_retains_recovery_when_retirement_capacity_refuses_admission(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let (published_gate, published, publish_resume) = gate();
    let (key, drain) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.attachment_storage.gates.published = Some(published_gate);
                let key = seed_source(
                    workspace,
                    owner("single"),
                    vec![file("single.txt", "text/plain", b"complete")],
                );
                workspace.retry_attachment_storage(&key, window, cx);
                (
                    key,
                    workspace
                        .attachment_storage
                        .take_drain()
                        .expect("single source coordinator"),
                )
            })
        })
        .expect("window");
    published
        .await
        .expect("complete source published before cancellation");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retired_attachment_blobs = (0u128..)
                .map(|value| {
                    BlobId::parse(&format!("{value:032x}")).expect("retirement fixture label")
                })
                .filter(|blob| blob.as_str() != key.as_str())
                .take(MAX_ENTRIES)
                .collect();
            workspace.discard_attachment_capture(&key, window, cx);
        })
    })
    .expect("window");
    publish_resume
        .send(())
        .expect("retire the canceled publication");
    drain.await;
    workspace.update(cx, |workspace, _| {
        let source = &workspace.attachment_storage.sources[&key];
        assert!(source.cancelled);
        assert_eq!(source.phase, SourcePhase::Published);
        assert!(
            workspace.attachments.contains_key(&key),
            "refused admission retains the descriptor and original bytes"
        );
        assert!(workspace.drafts.get(&owner("single")).is_none());
        assert!(workspace.has_uncommitted_attachment_storage());
    });
    let drain = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.retired_attachment_blobs.pop();
                workspace.retry_attachment_storage(&key, window, cx);
                workspace
                    .attachment_storage
                    .take_drain()
                    .expect("explicit canceled-source cleanup retry")
            })
        })
        .expect("window");
    drain.await;
    workspace.update(cx, |workspace, _| {
        assert!(!workspace.attachments.contains_key(&key));
        assert!(!workspace.has_uncommitted_attachment_storage());
        assert_eq!(workspace.attachment_storage.recovery_bytes, 0);
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn whole_source_capacity_is_reserved_before_any_hold_or_put(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let (_window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    workspace.update(cx, |workspace, _| {
        let maximum = || {
            let text = format!("{}\n[one.png]\n[two.png]\n", "界".repeat(500_000 - 32));
            assert!(text.encode_utf16().count() <= 500_000);
            let mut png = document().remove(1).content;
            png.resize(20 * 1024 * 1024, 0);
            vec![
                file("maximum.odt", "text/plain", text.as_bytes()),
                file("one.png", "image/png", &png),
                file("two.png", "image/png", &png),
            ]
        };
        let first = seed_source(workspace, owner("first"), maximum());
        let second = seed_source(workspace, owner("second"), maximum());
        assert!(workspace.attachment_storage.recovery_bytes <= MAX_RECOVERY_BYTES, "both approved maximum documents fit");
        assert!(workspace.attachment_storage.sources[&first].capacity > 40 * 1024 * 1024,
            "approved UTF-8 text plus cited images exceed the superseded per-source 40 MiB assumption");
        let holds = workspace.attachment_preparations.len();
        let mut refused = document();
        refused[0].content.resize(1024, b'x');
        assert!(matches!(workspace.capture_normalized_source(900, owner("refused"), refused), Err(AttachmentStorageFailure::Capacity)));
        assert_eq!(workspace.attachment_preparations.len(), holds, "no member of an unaccepted source gets a hold or put");
        assert!(workspace.attachments.is_empty());
        assert!(workspace.attachment_storage.sources.contains_key(&first));
        let removed = workspace.attachment_storage.sources.remove(&second).expect("explicit test-owned discard");
        workspace.attachment_storage.forget_source(&removed);
        workspace.attachment_preparations.retain(|hold| !removed.members.iter().any(|member| member.hold == *hold));
        drop(removed);
        assert!(workspace.capture_normalized_source(901, owner("smaller"), document()).is_ok(), "a fitting independent source is still admitted");
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn publication_rechecks_complete_catalog_and_new_draft_owner_counts_before_mutation(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (_window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let mut validator = fixture.validator();
    workspace.update(cx, |workspace, cx| {
        for value in 0..MAX_ENTRIES {
            let id = AttachmentId::from(format!("{value:032x}"));
            let blob = BlobId::parse(id.as_str()).expect("catalog fixture ID");
            workspace.attachments.insert(
                id,
                PreparedAttachment {
                    blob: BlobDescriptor::new(blob, 0, [0; 32]),
                    staging: None,
                },
            );
        }
        let key = seed_source(workspace, owner("document"), document());
        let source = activate_for_publication(workspace, &key);
        save_valid(&mut validator, &snapshot(workspace));
        let holds = workspace.attachment_preparations.clone();
        assert_eq!(
            workspace.publish_storage_source(&source, cx),
            Err(AttachmentStorageFailure::Capacity)
        );
        assert_eq!(workspace.attachments.len(), MAX_ENTRIES);
        assert_eq!(workspace.attachment_preparations, holds);
        assert!(workspace.drafts.get(&source.owner).is_none());
        save_valid(&mut validator, &snapshot(workspace));
        for value in 0..2 {
            workspace
                .attachments
                .remove(&AttachmentId::from(format!("{value:032x}")));
        }
        workspace
            .publish_storage_source(&source, cx)
            .expect("entire source fits after two slots become available");
        assert_eq!(workspace.attachments.len(), MAX_ENTRIES);
        assert_eq!(
            workspace
                .drafts
                .get(&source.owner)
                .expect("whole document")
                .content
                .files
                .len(),
            2
        );
        save_valid(&mut validator, &snapshot(workspace));

        workspace.drafts = Default::default();
        workspace.attachments.clear();
        workspace.attachment_preparations.clear();
        workspace.attachment_storage = Default::default();
        for value in 0..MAX_ENTRIES {
            workspace
                .drafts
                .save(owner(&format!("existing-{value}")), Draft::new("retained"));
        }
        let key = seed_source(workspace, owner("new-owner"), document());
        let source = activate_for_publication(workspace, &key);
        let holds = workspace.attachment_preparations.clone();
        save_valid(&mut validator, &snapshot(workspace));
        assert_eq!(
            workspace.publish_storage_source(&source, cx),
            Err(AttachmentStorageFailure::Capacity)
        );
        assert_eq!(workspace.drafts.len(), MAX_ENTRIES);
        assert_eq!(workspace.attachment_preparations, holds);
        assert!(workspace.attachments.is_empty());
        save_valid(&mut validator, &snapshot(workspace));
        workspace.drafts.take(&owner("existing-0"));
        workspace
            .publish_storage_source(&source, cx)
            .expect("one complete new owner fits");
        assert_eq!(workspace.drafts.len(), MAX_ENTRIES);
        save_valid(&mut validator, &snapshot(workspace));
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn whole_snapshot_encoded_limit_refuses_atomic_publication_without_changing_valid_state(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (_window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let mut validator = fixture.validator();
    workspace.update(cx, |workspace, cx| {
        for value in 0..7 {
            workspace.drafts.save(
                owner(&format!("escaped-{value}")),
                Draft::new("\n".repeat(MAX_TEXT)),
            );
        }
        workspace
            .drafts
            .save(owner("plain"), Draft::new("a".repeat(MAX_TEXT)));
        workspace.drafts.save(owner("fill"), Draft::new("a"));
        let key = seed_source(workspace, owner("document"), document());
        let source = activate_for_publication(workspace, &key);
        save_valid(&mut validator, &snapshot(workspace));
        let baseline = fs::metadata(fixture.0.join("validation.json"))
            .expect("encoded baseline")
            .len() as usize;
        let fill = 16 * 1024 * 1024 - baseline - 32 + 1;
        assert!(
            fill <= MAX_TEXT,
            "fixture's last draft remains individually valid"
        );
        workspace
            .drafts
            .save(owner("fill"), Draft::new("a".repeat(fill)));
        save_valid(&mut validator, &snapshot(workspace));
        let original_len = fs::metadata(fixture.0.join("validation.json"))
            .expect("near-limit snapshot")
            .len();
        assert!(
            validator.save(&candidate(workspace, &source)).is_err(),
            "actual canonical encoding rejects the prospective whole source"
        );
        assert_eq!(
            workspace.publish_storage_source(&source, cx),
            Err(AttachmentStorageFailure::Capacity)
        );
        assert!(workspace.attachments.is_empty());
        assert_eq!(workspace.attachment_preparations.len(), 2);
        assert!(workspace.drafts.get(&source.owner).is_none());
        assert_eq!(
            workspace
                .drafts
                .get(&owner("fill"))
                .expect("original source")
                .text
                .len(),
            fill
        );
        save_valid(&mut validator, &snapshot(workspace));
        assert_eq!(
            fs::metadata(fixture.0.join("validation.json"))
                .expect("unchanged snapshot")
                .len(),
            original_len
        );
        workspace
            .drafts
            .save(owner("fill"), Draft::new("a".repeat(fill - 4096)));
        workspace
            .publish_storage_source(&source, cx)
            .expect("complete group fits after unrelated content is explicitly reduced");
        assert_eq!(
            workspace
                .drafts
                .get(&source.owner)
                .expect("complete source")
                .content
                .files
                .len(),
            2
        );
        assert!(workspace.attachment_preparations.is_empty());
        save_valid(&mut validator, &snapshot(workspace));
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn initial_hold_encoded_overflow_leaves_existing_saves_and_sources_valid(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (_window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let mut validator = fixture.validator();
    workspace.update(cx, |workspace, _| {
        let sibling = seed_source(workspace, owner("sibling"), document());
        for value in 0..7 {
            workspace.drafts.save(
                owner(&format!("escaped-{value}")),
                Draft::new("\n".repeat(MAX_TEXT)),
            );
        }
        workspace
            .drafts
            .save(owner("plain"), Draft::new("a".repeat(MAX_TEXT)));
        workspace.drafts.save(owner("fill"), Draft::new("a"));
        save_valid(&mut validator, &snapshot(workspace));
        let baseline = fs::metadata(fixture.0.join("validation.json"))
            .expect("encoded baseline")
            .len() as usize;
        let fill = 16 * 1024 * 1024 - baseline - 32 + 1;
        assert!(
            fill <= MAX_TEXT,
            "every retained draft remains individually valid"
        );
        workspace
            .drafts
            .save(owner("fill"), Draft::new("a".repeat(fill)));
        save_valid(&mut validator, &snapshot(workspace));
        let original =
            fs::read(fixture.0.join("validation.json")).expect("valid near-limit journal");
        let holds = workspace.attachment_preparations.clone();
        let recovery_bytes = workspace.attachment_storage.recovery_bytes;
        let held_count = workspace.attachment_storage.holds.len();
        let member_count = workspace.attachment_storage.members.len();
        assert!(
            matches!(
                workspace.capture_normalized_source(900, owner("refused"), document()),
                Err(AttachmentStorageFailure::Capacity)
            ),
            "the canonical encoded ceiling rejects the initial source holds"
        );
        assert_eq!(workspace.attachment_preparations, holds);
        assert_eq!(workspace.attachment_storage.recovery_bytes, recovery_bytes);
        assert_eq!(workspace.attachment_storage.holds.len(), held_count);
        assert_eq!(workspace.attachment_storage.members.len(), member_count);
        assert!(workspace.attachment_storage.active.is_none());
        assert!(workspace.attachment_storage.sources.contains_key(&sibling));
        assert!(workspace.attachments.is_empty());
        save_valid(&mut validator, &snapshot(workspace));
        assert_eq!(
            fs::read(fixture.0.join("validation.json")).expect("unchanged valid snapshot"),
            original
        );

        workspace
            .drafts
            .save(owner("fill"), Draft::new("a".repeat(fill - 4096)));
        let accepted = seed_source(workspace, owner("accepted"), document());
        assert!(workspace.attachment_storage.sources.contains_key(&sibling));
        assert!(workspace.attachment_storage.sources.contains_key(&accepted));
        save_valid(&mut validator, &snapshot(workspace));
    });
    finish_worker(&workspace, cx).await;
}

#[gpui_kit::test]
async fn initial_hold_semantic_refusal_and_exact_publication_checks_preserve_the_owner(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let (_window, workspace) = open(cx, &fixture);
    cx.condition(&workspace, |workspace, _| workspace.persistence_loaded())
        .await;
    let mut validator = fixture.validator();
    workspace.update(cx, |workspace, cx| {
        let invalid = DraftKey::new(
            RuntimeId::from("invalid\nruntime"),
            "/captured/project",
            DraftTarget::NewSession(DraftId::from("invalid")),
        )
        .expect("core key carries the untrusted runtime for journal validation");
        assert!(matches!(
            workspace.capture_normalized_source(900, invalid, document()),
            Err(AttachmentStorageFailure::Capacity)
        ));
        assert!(workspace.attachment_preparations.is_empty());
        assert!(workspace.attachment_storage.holds.is_empty());
        assert!(workspace.attachment_storage.members.is_empty());
        assert_eq!(workspace.attachment_storage.recovery_bytes, 0);
        save_valid(&mut validator, &snapshot(workspace));

        let key = seed_source(workspace, owner("document"), document());
        let source = activate_for_publication(workspace, &key);
        let mut draft = Draft::new("retained".repeat(100_000));
        draft
            .content
            .context
            .push(openchamber_native_core::draft_content::DraftContext {
                id: openchamber_native_core::ids::ContextItemId::from("large-context"),
                text: "c".repeat(MAX_TEXT),
                description: None,
                metadata_json: None,
                instructions: None,
            });
        let text_pointer = draft.text.as_ptr();
        let context_pointer = draft.content.context[0].text.as_ptr();
        workspace.drafts.save(source.owner.clone(), draft);
        let mut publication: Vec<_> = source
            .members
            .iter()
            .map(|member| {
                (
                    &member.hold,
                    member.descriptor.expect("put receipt fixture"),
                    member.metadata(&source.group),
                )
            })
            .collect();
        let mut stale = source.members[0].hold.clone();
        stale.generation += 1;
        publication[0].0 = &stale;
        assert!(
            workspace
                .validate_attachment_bundle_publication(&source.owner, &publication)
                .is_err()
        );
        publication[0].0 = &source.members[0].hold;
        publication[0].2.byte_len = Some(0);
        assert!(
            workspace
                .validate_attachment_bundle_publication(&source.owner, &publication)
                .is_err()
        );
        assert!(workspace.attachments.is_empty());
        assert_eq!(workspace.attachment_preparations.len(), 2);
        workspace
            .publish_storage_source(&source, cx)
            .expect("valid complete source publishes atomically");
        let published = workspace
            .drafts
            .get(&source.owner)
            .expect("retained draft gains the entire bundle");
        assert_eq!(
            published.text.as_ptr(),
            text_pointer,
            "publication moves existing text"
        );
        assert_eq!(
            published.content.context[0].text.as_ptr(),
            context_pointer,
            "publication moves existing context"
        );
        assert_eq!(published.content.files.len(), 2);
        assert_eq!(workspace.attachments.len(), 2);
        assert!(workspace.attachment_preparations.is_empty());
        save_valid(&mut validator, &snapshot(workspace));
    });
    finish_worker(&workspace, cx).await;
}
