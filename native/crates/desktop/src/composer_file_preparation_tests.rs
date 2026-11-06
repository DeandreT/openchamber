//! Captured authority, whole-bundle publication and exact retry contracts.

use std::fs;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use openchamber_native_core::draft::{Draft, DraftTarget};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::queue_manifest::{QueueLocalItem, QueuePromptParts};

use crate::attachment_io::AttachmentJournal;
use crate::attachment_spool::{BlobReceipt, SpoolCommit};
use crate::draft_companion::CompanionCapture;
use crate::mutation_journal::{JournalCommit, JournalSnapshot, JournalView, LoadedJournal};
use crate::network::Network;
use crate::queue_capture::QueueNativeSource;
use crate::queue_storage::{QueueConfiguration, QueueOwner};

use super::*;

fn owner() -> DraftKey {
    DraftKey::new(
        RuntimeId::from("mention-host"),
        "/project",
        DraftTarget::Session(SessionId::from("ses_mentions")),
    )
    .expect("captured draft owner")
}

fn configuration() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "model".into(),
        agent: None,
        variant: None,
    }
}

fn blob(value: u128) -> BlobId {
    BlobId::parse(&format!("{value:032x}")).expect("local test label")
}

fn local_capture(text: &str, files: Vec<QueueLocalFile>) -> Arc<QueueLocalCapture> {
    Arc::new(
        QueueLocalCapture::new(
            "/project",
            QueueLocalItem {
                content: text.to_owned(),
                text: text.to_owned(),
                files,
                send_config: configuration(),
                prompt_parts: QueuePromptParts::default(),
            },
            Vec::new(),
            sha256,
        )
        .expect("immutable source bytes"),
    )
}

fn operation(text: &str) -> QueueOperation {
    QueueOperation::local_send(
        QueueOperationId::new("mention_original").expect("operation ID"),
        QueueOwner::new(
            owner().runtime(),
            "/project",
            &SessionId::from("ses_mentions"),
            &owner(),
        )
        .expect("exact owner"),
        QueueConfiguration::capture(&configuration()),
        MessageId::from("msg_mention_original"),
        CompanionCapture {
            native: QueueNativeSource {
                owner: owner(),
                draft: Draft::new(text),
            },
            references: Vec::new(),
        },
        None,
        Vec::new(),
        Vec::new(),
    )
    .expect("typed source intent")
}

fn base_plan(operation: &QueueOperation, capture: &QueueLocalCapture) -> QueueComposerPreparation {
    QueueComposerPreparation::new(
        &owner(),
        *operation.source_sha256().expect("original source seal"),
        7,
        3,
        vec![QueuePreparationSource {
            identity: QueuePreparationSourceId::Source(0),
            byte_len: capture.bytes().len() as u64,
            sha256: capture.sha256(),
        }],
        Vec::new(),
        Vec::new(),
    )
}

fn source(text: &str) -> Arc<CapturedFileMentionSource> {
    let capture = local_capture(text, Vec::new());
    Arc::new(
        CapturedFileMentionSource::new(
            &base_plan(&operation(text), &capture),
            QueuePreparationSourceId::Source(0),
            capture,
        )
        .expect("source-bound text"),
    )
}

fn request_from(
    source: Arc<CapturedFileMentionSource>,
    token: &str,
    path: &str,
) -> CapturedFileMention {
    let text = &source.capture.item().content;
    let byte = text.find(token).expect("explicit token exists in source");
    let start = text[..byte].encode_utf16().count() as u64;
    let end = start + token.encode_utf16().count() as u64;
    CapturedFileMention::new(
        source,
        QueueMention {
            start,
            end,
            text: token.to_owned(),
        },
        path,
    )
    .expect("explicit read-on-send request")
}

fn request(filename: &str) -> CapturedFileMention {
    let token = format!("@{filename}");
    request_from(source(&token), &token, &format!("/project/{filename}"))
}

fn png() -> Vec<u8> {
    use image::ImageEncoder;

    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[2, 3, 5], 1, 1, image::ExtendedColorType::Rgb8)
        .expect("synthetic PNG");
    bytes
}

fn document() -> Vec<u8> {
    let png = png();
    let entries = [
        ("word/document.xml", br#"<w:document><w:p><w:t>Before</w:t></w:p><w:p><a:blip r:embed="image"/></w:p><w:p><w:t>After</w:t></w:p></w:document>"#.as_slice()),
        ("word/_rels/document.xml.rels", br#"<Relationships><Relationship Id="image" Target="media/photo.png" Type="image"/></Relationships>"#.as_slice()),
        ("word/media/photo.png", png.as_slice()),
    ];
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (path, bytes) in entries {
        writer
            .start_file(path, zip::write::SimpleFileOptions::default())
            .expect("synthetic ZIP entry");
        writer.write_all(bytes).expect("synthetic ZIP content");
    }
    writer.finish().expect("synthetic document").into_inner()
}

fn normalized_document() -> NormalizedFileMention {
    normalize_bytes(request("report.docx"), document(), Vec::new(), None)
        .expect("real document intake")
}

#[test]
fn only_the_approved_read_on_send_extensions_enter_byte_intake() {
    for extension in [
        "docx", "pptx", "xlsx", "odt", "odp", "ods", "png", "jpg", "jpeg", "gif", "webp", "heic",
        "heif",
    ] {
        let filename = format!("selected.{}", extension.to_ascii_uppercase());
        assert!(reads_on_send(&filename));
        request(&filename);
    }
    for filename in [
        "selected.pdf",
        "selected.PDF",
        "selected.txt",
        "selected.svg",
        "selected.bin",
        "selected",
    ] {
        assert!(!reads_on_send(filename));
        let token = format!("@{filename}");
        assert_eq!(
            CapturedFileMention::new(
                source(&token),
                QueueMention {
                    start: 0,
                    end: token.encode_utf16().count() as u64,
                    text: token
                },
                &format!("/project/{filename}")
            )
            .expect_err("reference-only file is rejected"),
            FileMentionPreparationError::ReferenceOnly,
            "reference-only files cannot be substituted with inline bytes"
        );
    }
}

#[test]
fn utf16_mentions_use_the_exact_shared_captured_text_and_reject_stale_positions() {
    let text = format!("{}👩 @selected.PNG", "a".repeat(12_000));
    let source = source(&text);
    let first = request_from(
        Arc::clone(&source),
        "@selected.PNG",
        "/project/selected.PNG",
    );
    let second = request_from(
        Arc::clone(&source),
        "@selected.PNG",
        "/project/selected.PNG",
    );
    assert!(
        Arc::ptr_eq(&first.source.capture, &second.source.capture),
        "requests share the complete original capture"
    );
    assert!(
        source.boundaries.len() < 8,
        "sparse indexing does not duplicate a large source per mention"
    );
    assert_eq!(first.mention.start, 12_003);
    assert_eq!(
        source.byte_offset(12_001),
        None,
        "an offset inside the surrogate pair is not a text boundary"
    );
    let mut wrong = QueueMention {
        start: first.mention.start,
        end: first.mention.end,
        text: first.mention.text.clone(),
    };
    wrong.start -= 1;
    assert_eq!(
        CapturedFileMention::new(Arc::clone(&source), wrong, "/project/selected.PNG")
            .expect_err("shifted mention is rejected"),
        FileMentionPreparationError::InvalidMention
    );
    let wrong_name = QueueMention {
        start: first.mention.start,
        end: first.mention.end,
        text: first.mention.text.clone(),
    };
    assert_eq!(
        CapturedFileMention::new(source, wrong_name, "/project/unrelated.docx")
            .expect_err("mismatched basename is rejected"),
        FileMentionPreparationError::InvalidMention,
        "the selected path must retain the explicitly mentioned basename"
    );
}

#[test]
fn attachment_metadata_and_old_model_facing_paths_cannot_authorize_a_read() {
    let old = QueueLocalFile {
        name: "/private/old.png".into(),
        mime_type: "image/png".into(),
        byte_len: 4,
        sha256: sha256(b"held"),
        description: Some("existing attachment".into()),
        mention: Some(QueueMention {
            start: 0,
            end: 8,
            text: "@old.png".into(),
        }),
    };
    let capture = local_capture("plain prose", vec![old]);
    let operation = operation("plain prose");
    let plan = base_plan(&operation, &capture);
    let captured_source = Arc::new(
        CapturedFileMentionSource::new(&plan, QueuePreparationSourceId::Source(0), capture)
            .expect("held files remain independent of typed source"),
    );
    assert_eq!(
        CapturedFileMention::new(
            captured_source,
            QueueMention {
                start: 0,
                end: 8,
                text: "@old.png".into()
            },
            "/private/old.png"
        )
        .expect_err("path outside the project is rejected"),
        FileMentionPreparationError::InvalidMention
    );
    assert!(
        plan.files.is_empty(),
        "existing file metadata creates no derived read or byte plan"
    );
    for text in ["mail@old.png", "@old.png-suffix"] {
        let captured = source(text);
        let start = text.find('@').expect("at sign") as u64;
        assert_eq!(
            CapturedFileMention::new(
                captured,
                QueueMention {
                    start,
                    end: start + 8,
                    text: "@old.png".into()
                },
                "/project/old.png"
            )
            .expect_err("substring mention is rejected"),
            FileMentionPreparationError::InvalidMention,
            "a substring of another token is not a new typed file mention"
        );
    }
}

#[test]
fn source_and_server_scope_must_match_before_a_raw_read_can_start() {
    let text = "@selected.png";
    let capture = local_capture(text, Vec::new());
    let mut operation = operation(text);
    let plan = base_plan(&operation, &capture);
    let request = request_from(
        Arc::new(
            CapturedFileMentionSource::new(&plan, QueuePreparationSourceId::Source(0), capture)
                .expect("captured source"),
        ),
        text,
        "/project/selected.png",
    );
    let id = operation.id().clone();
    let mut snapshot = JournalSnapshot::default();
    snapshot
        .queue
        .admit_operation(operation, |_| Ok(()))
        .map_err(|(error, _)| error)
        .expect("admitted operation");
    snapshot
        .queue
        .plan_composer_preparation(
            &id,
            plan,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, _| Ok(()),
            |_, _| Ok(()),
        )
        .expect("admitted source authority");
    operation = snapshot.queue.operation(&id).expect("operation").clone();
    let network = Network::new().expect("test-owned runtime, no requests");
    let client = ServerClient::new(
        Endpoint::parse("http://localhost:48123/base").expect("captured endpoint"),
        Credential::None,
    )
    .expect("captured client");
    let exact = QueueServer::new(
        RuntimeId::from("mention-host"),
        client.clone(),
        network.handle(),
        SessionId::from("ses_mentions"),
        "/project",
    )
    .expect("exact scope");
    assert_eq!(validate_scope(&operation, &exact, &request), Ok(()));
    for (runtime, directory, session) in [
        ("other-host", "/project", "ses_mentions"),
        ("mention-host", "/other", "ses_mentions"),
        ("mention-host", "/project", "ses_other"),
    ] {
        let wrong = QueueServer::new(
            RuntimeId::from(runtime),
            client.clone(),
            network.handle(),
            SessionId::from(session),
            directory,
        )
        .expect("independent captured scope");
        assert_eq!(
            validate_scope(&operation, &wrong, &request),
            Err(FileMentionPreparationError::WrongOwner)
        );
    }
    let mut changed_request = request.clone();
    let mut newer = snapshot
        .queue
        .operation(&id)
        .expect("operation")
        .composer_preparation()
        .expect("plan")
        .clone();
    newer.revision += 1;
    changed_request.source = Arc::new(
        CapturedFileMentionSource::new(
            &newer,
            QueuePreparationSourceId::Source(0),
            Arc::clone(&request.source.capture),
        )
        .expect("different captured revision"),
    );
    assert_eq!(
        validate_scope(&operation, &exact, &changed_request),
        Err(FileMentionPreparationError::CaptureConflict)
    );
    let mut wrong_source = newer;
    wrong_source.sources[0].sha256 = sha256(b"unrelated");
    assert_eq!(
        CapturedFileMentionSource::new(
            &wrong_source,
            QueuePreparationSourceId::Source(0),
            Arc::clone(&request.source.capture)
        )
        .expect_err("conflicting capture is rejected"),
        FileMentionPreparationError::CaptureConflict
    );
}

#[test]
fn sealed_inputs_and_missing_source_authority_cannot_start_preparation_again() {
    let capture = local_capture("@selected.png", Vec::new());
    let plan = base_plan(&operation("@selected.png"), &capture);
    assert_eq!(
        CapturedFileMentionSource::new(
            &plan,
            QueuePreparationSourceId::Source(1),
            Arc::clone(&capture)
        )
        .expect_err("missing authority is rejected"),
        FileMentionPreparationError::MissingAuthority
    );
    let mut value = serde_json::to_value(&plan).expect("persisted preparation shape");
    value["inputs_object"] = serde_json::json!("already_sealed_inputs");
    let sealed: QueueComposerPreparation =
        serde_json::from_value(value).expect("restored one-time input binding");
    assert_eq!(
        CapturedFileMentionSource::new(&sealed, QueuePreparationSourceId::Source(0), capture)
            .expect_err("frozen inputs are rejected"),
        FileMentionPreparationError::FrozenInputs,
        "a frozen input capture must be reused without another raw read or conversion"
    );
}

#[test]
fn complete_document_outputs_keep_text_first_citations_order_and_reserved_names() {
    let normalized = normalized_document();
    assert_eq!(normalized.output_count(), 2);
    assert_eq!(normalized.outputs[0].file.filename, "report.docx");
    assert_eq!(normalized.outputs[0].file.media_type, "text/plain");
    assert_eq!(normalized.outputs[1].file.media_type, "image/png");
    let citation = normalized.outputs[1].file.filename.clone();
    let text = std::str::from_utf8(&normalized.outputs[0].file.content).expect("extracted text");
    assert!(
        text.contains(&format!("Before\n\n[{citation}]\n\nAfter")),
        "document text and image citations retain intake order"
    );
    let reserved = normalize_bytes(
        request("report.docx"),
        document(),
        vec![citation.to_ascii_uppercase()],
        None,
    )
    .expect("existing names are forwarded to intake");
    assert_ne!(
        reserved.outputs[1].file.filename, citation,
        "image citations reserve names case-insensitively"
    );
    let plan = normalized
        .plan(&[blob(1), blob(2)], Some("source-document".into()))
        .expect("whole document plan");
    assert_eq!(plan.len(), normalized.output_count());
    assert_eq!(plan[0].output_ordinal, 0);
    assert_eq!(plan[1].output_ordinal, 1);
    for (file, output) in plan.iter().zip(&normalized.outputs) {
        assert_eq!(file.raw_source_byte_len, normalized.raw_source_byte_len());
        assert_eq!(&file.raw_source_sha256, normalized.raw_source_sha256());
        assert_eq!(file.sha256, sha256(&output.file.content));
        assert_eq!(file.byte_len, output.file.content.len() as u64);
        assert_eq!(file.document_group.as_deref(), Some("source-document"));
    }
    assert!(
        normalized
            .plan(&[blob(1)], Some("source-document".into()))
            .is_err(),
        "no subset plan is admitted"
    );
    assert!(
        normalized.plan(&[blob(1), blob(2)], None).is_err(),
        "document provenance survives single or multiple outputs"
    );
    assert!(
        normalized
            .plan(&[blob(1), blob(1)], Some("source-document".into()))
            .is_err()
    );
    assert!(
        normalized
            .plan(&[blob(1), blob(2)], Some(blob(1).as_str().into()))
            .is_err()
    );
}

#[test]
fn raw_bounds_and_changed_raw_input_fail_before_conversion() {
    assert_eq!(
        normalize_bytes(
            request("selected.png"),
            vec![0; MAX_INLINE_ATTACHMENT_BYTES + 1],
            Vec::new(),
            None
        )
        .expect_err("oversized bytes are rejected"),
        FileMentionPreparationError::InputTooLarge
    );
    let normalized = normalized_document();
    let expected = Some((
        normalized.raw_source_byte_len(),
        *normalized.raw_source_sha256(),
    ));
    assert_eq!(
        normalize_bytes(
            request("report.docx"),
            b"changed and invalid ZIP".to_vec(),
            Vec::new(),
            expected
        )
        .expect_err("changed source is rejected"),
        FileMentionPreparationError::SourceChanged,
        "a changed raw source cannot reach document or HEIC conversion on retry"
    );
    let exact = normalize_bytes(request("report.docx"), document(), Vec::new(), expected)
        .expect("exact raw source retry");
    assert_eq!(exact.raw_source_sha256(), normalized.raw_source_sha256());
}

#[test]
fn retry_compares_every_saved_descriptor_including_already_cataloged_siblings() {
    let normalized = normalized_document();
    let files = normalized
        .plan(&[blob(11), blob(12)], Some("document-group".into()))
        .expect("full bundle");
    let mut saved: Vec<_> = files.into_iter().enumerate().collect();
    saved[0].1.state = QueueDerivedFileState::Cataloged;
    assert_eq!(match_outputs(&normalized, &saved), Ok(()));
    for mutation in 0..7 {
        let mut changed = saved.clone();
        let file = &mut changed[0].1;
        match mutation {
            0 => file.raw_source_sha256 = sha256(b"changed raw"),
            1 => file.raw_source_byte_len += 1,
            2 => file.sha256 = sha256(b"changed output"),
            3 => file.byte_len += 1,
            4 => file.name = "replacement.docx".into(),
            5 => file.mime_type = "application/pdf".into(),
            _ => {
                changed.pop();
            }
        }
        assert_eq!(
            match_outputs(&normalized, &changed),
            Err(FileMentionPreparationError::SourceChanged),
            "an existing receipt never permits replacing or omitting an original output"
        );
    }
}

fn verify_source(
    plan: &QueueComposerPreparation,
    request: &CapturedFileMention,
) -> Result<(), JournalError> {
    if !plan.sources.contains(&request.source.source) {
        return Err(JournalError::Invalid("source fingerprint changed"));
    }
    for file in &plan.files {
        if !request.source.validates(&QueueMention {
            start: file.mention.start,
            end: file.mention.end,
            text: file.mention.text.clone(),
        }) {
            return Err(JournalError::Invalid("mention differs from captured input"));
        }
    }
    Ok(())
}

fn snapshot_for(normalized: &NormalizedFileMention) -> JournalSnapshot {
    let text = &normalized.request.source.capture.item().content;
    let operation = operation(text);
    let id = operation.id().clone();
    let mut plan = base_plan(&operation, &normalized.request.source.capture);
    plan.files = normalized
        .plan(&[blob(21), blob(22)], Some("held-document".into()))
        .expect("full outputs");
    let mut snapshot = JournalSnapshot::default();
    snapshot
        .queue
        .admit_operation(operation, |_| Ok(()))
        .map_err(|(error, _)| error)
        .expect("operation admission");
    let mut queue = std::mem::take(&mut snapshot.queue);
    queue
        .plan_composer_preparation(
            &id,
            plan,
            &snapshot.attachments,
            &mut snapshot.preparations,
            |_, plan| verify_source(plan, &normalized.request),
            |_, _| Ok(()),
        )
        .expect("all output holds admitted together");
    snapshot.queue = queue;
    JournalView::from(&snapshot)
        .validate()
        .expect("full canonical snapshot");
    snapshot
}

fn publish(
    snapshot: &mut JournalSnapshot,
    ordinal: usize,
    receipt: &BlobReceipt,
) -> Result<(), JournalError> {
    let id = snapshot.queue.operations()[0].id().clone();
    let mut queue = std::mem::take(&mut snapshot.queue);
    let mut attachments = std::mem::take(&mut snapshot.attachments);
    let mut holds = std::mem::take(&mut snapshot.preparations);
    let result = queue.publish_composer_derived_file(
        &id,
        7,
        ordinal,
        receipt,
        &mut attachments,
        &mut holds,
        |candidate, catalog, preparations| {
            let mut view = JournalView::from(&*snapshot);
            view.queue = candidate;
            view.attachments = catalog;
            view.preparations = preparations;
            view.validate()
        },
    );
    snapshot.queue = queue;
    snapshot.attachments = attachments;
    snapshot.preparations = holds;
    result
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from("/tmp/opencode").join(format!(
            "mention-preparation-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).expect("private test-owned profile");
        Self(path)
    }

    fn journal(&self) -> PathBuf {
        self.0.join("journal.json")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
    assert!(
        matches!(journal.save(snapshot), Ok(JournalCommit::Durable)),
        "full ownership barrier is durable"
    );
}

#[test]
fn partial_publication_retains_exact_successes_and_holds_across_restart_and_retry() {
    let normalized = normalized_document();
    let mut snapshot = snapshot_for(&normalized);
    assert_eq!(
        snapshot.preparations.len(),
        2,
        "whole document plan exists before its first put"
    );
    let scratch = Scratch::new();
    let (mut journal, _) =
        AttachmentJournal::open(&scratch.journal()).expect("test profile journal");
    assert!(
        journal
            .put(
                &snapshot.preparations[0],
                &normalized.outputs[0].file.content
            )
            .is_err(),
        "an admitted in-memory plan grants no disk authority before its save"
    );
    save(&mut journal, &snapshot);
    let first = journal
        .put(
            &snapshot.preparations[0],
            &normalized.outputs[0].file.content,
        )
        .expect("first durable output");
    publish(&mut snapshot, 0, &first).expect("first catalog receipt");
    save(&mut journal, &snapshot);
    let first_descriptor = first.descriptor;
    let before = serde_json::to_vec(&snapshot.queue).expect("partial durable plan");
    let wrong = BlobReceipt {
        descriptor: BlobDescriptor::new(blob(22), 7, sha256(b"changed")),
        commit: SpoolCommit::Durable,
    };
    assert!(publish(&mut snapshot, 1, &wrong).is_err());
    assert_eq!(
        serde_json::to_vec(&snapshot.queue).expect("unchanged partial plan"),
        before
    );
    assert_eq!(
        snapshot.attachments.len(),
        1,
        "a later failure cannot erase a successful publication"
    );
    drop(journal);
    let (mut journal, loaded) =
        AttachmentJournal::open(&scratch.journal()).expect("explicit recovery load");
    let LoadedJournal::Present(mut snapshot) = loaded else {
        panic!("durable partial preparation");
    };
    let id = snapshot.queue.operations()[0].id().clone();
    let saved = bundle(
        snapshot
            .queue
            .operation(&id)
            .expect("operation")
            .composer_preparation()
            .expect("plan"),
        &normalized.request,
    )
    .expect("exact saved bundle");
    assert_eq!(match_outputs(&normalized, &saved), Ok(()));
    assert_eq!(
        cataloged_descriptor(&snapshot.attachments, &saved[0].1),
        Ok(first_descriptor)
    );
    assert!(
        cataloged_descriptor(&snapshot.attachments, &saved[1].1).is_err(),
        "partial bundles have no complete typed result"
    );
    assert!(
        snapshot
            .queue
            .operation(&id)
            .expect("operation")
            .composer_preparation()
            .expect("plan")
            .inputs_object()
            .is_none()
    );
    let second_descriptor = saved[1].1.descriptor().expect("saved identity and digest");
    let second = journal
        .resume_attachment(
            &snapshot.preparations[0],
            &second_descriptor,
            &normalized.outputs[1].file.content,
        )
        .expect("explicit exact retry of unfinished output");
    publish(&mut snapshot, 1, &second).expect("remaining catalog receipt");
    save(&mut journal, &snapshot);
    assert!(snapshot.preparations.is_empty());
    assert_eq!(snapshot.attachments.len(), 2);
    assert_eq!(
        snapshot
            .attachments
            .get(blob(21).as_str())
            .expect("first success retained")
            .blob,
        first_descriptor
    );
    let saved = bundle(
        snapshot
            .queue
            .operation(&id)
            .expect("operation")
            .composer_preparation()
            .expect("plan"),
        &normalized.request,
    )
    .expect("complete saved bundle");
    let outputs: Vec<_> = saved
        .into_iter()
        .map(|(_, file)| {
            prepared_output(&snapshot.attachments, &normalized.request, file)
                .expect("exact durable output binding")
        })
        .collect();
    assert_eq!(
        outputs[0]
            .file
            .mention
            .as_ref()
            .map(|mention| mention.text.as_str()),
        Some("@report.docx")
    );
    assert!(
        outputs[1].file.mention.is_none(),
        "cited document images do not duplicate the original typed anchor"
    );
}
