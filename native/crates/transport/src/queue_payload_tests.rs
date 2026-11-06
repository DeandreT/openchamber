//! Payload-v1 capture, compact handoff and ownership boundary contracts.

use super::*;
use crate::credential::Credential;

const DIRECTORY: &str = "/home/synthetic/large context";

// Fixed output from the backend's parseManifestItem, makePayloadIndex,
// sealPayloadPointer and parsePayloadPointer, with Node crypto SHA-256. The
// capture has 99 ASCII units followed by an emoji; slice(0, 100) splits its pair.
const BACKEND_CAPTURE: &str = r#"{"directory":"/home/synthetic/large context","item":{"format":"manifest-v1","content":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa😀","text":"deliver","attachmentManifest":[],"sendConfig":{"providerID":"p","modelID":"m","agent":"build","variant":"high"}}}"#;
const BACKEND_CAPTURE_SHA: &str =
    "46ae954f722b9f1f703f1f61de20e71a4e02a58ee91e8928f72eb8a892f083cb";
const BACKEND_INDEXED: &str = r#"{"format":"payload-v1","payloadManifest":{"version":1,"encoding":"utf-8-json","totalBytes":313,"sha256":"46ae954f722b9f1f703f1f61de20e71a4e02a58ee91e8928f72eb8a892f083cb","chunks":[{"ordinal":0,"blobId":"e88336f2bd647922ae968f8d8248b6c0b791ba7c8e4ddbfafcbc41b2afd62e2d","requestId":"upload_original","attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":313,"sha256":"46ae954f722b9f1f703f1f61de20e71a4e02a58ee91e8928f72eb8a892f083cb"}]},"payloadIndex":{"directory":"/home/synthetic/large context","sendConfig":{"providerID":"p","modelID":"m","agent":"build","variant":"high"},"preview":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ud83d...","entryCount":0,"contextIds":[],"attachmentBlobIds":[],"promptBytes":37,"admissionBytes":37,"structuredBytes":11},"payloadIndexHash":"ba47e45b1ece7d66740f1d3f90aed2a443e09077e5fe316e4f69e7a032ed1e94"}"#;
const BACKEND_BINDING: &str = r#"{"payloadManifest":{"version":1,"encoding":"utf-8-json","totalBytes":313,"sha256":"46ae954f722b9f1f703f1f61de20e71a4e02a58ee91e8928f72eb8a892f083cb","chunks":[{"attachmentId":"payload-0","filename":"payload-0.json","mimeType":"application/json","size":313,"sha256":"46ae954f722b9f1f703f1f61de20e71a4e02a58ee91e8928f72eb8a892f083cb","blobId":"e88336f2bd647922ae968f8d8248b6c0b791ba7c8e4ddbfafcbc41b2afd62e2d","requestId":"upload_original","ordinal":0}]},"payloadIndex":{"directory":"/home/synthetic/large context","sendConfig":{"providerID":"p","modelID":"m","agent":"build","variant":"high"},"preview":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ud83d...","entryCount":0,"contextIds":[],"attachmentBlobIds":[],"promptBytes":37,"admissionBytes":37,"structuredBytes":11}}"#;
const BACKEND_INDEX_SHA: &str = "ba47e45b1ece7d66740f1d3f90aed2a443e09077e5fe316e4f69e7a032ed1e94";

// Fixed output from encodeLegacyCapture and compactLegacyPayloadItem. Keep the
// expected number spellings independent of Rust's serializer and test hasher.
const BACKEND_NUMERIC_INPUT: &str =
    "[1.0,-0.0,1000,1e20,1e21,1000000000000000100,0.0000001,0.000001,1.2345]";
const BACKEND_NUMERIC_COMPACT: &str = "[1,0,1e3,1e20,1e21,1000000000000000100,1e-7,1e-6,1.2345]";
const BACKEND_LEGACY_CAPTURE: &str = r#"{"directory":"/home/synthetic/large context","item":{"context":[{"kind":"context","text":"padding","metadata":{"numbers":[1,0,1e3,1e20,1e21,1000000000000000100,1e-7,1e-6,1.2345]}}],"sendConfig":{"providerID":"p","modelID":"m","agent":"build","variant":"high"}}}"#;

// This is a fixed byte oracle, not a SHA implementation. It refuses every
// preimage other than the independent Node fixture. Desktop's real SHA callback
// must also run against these bytes during integration qualification.
fn backend_fixture_digest(bytes: &[u8]) -> [u8; 32] {
    let digest = if bytes == BACKEND_BINDING.as_bytes() {
        BACKEND_INDEX_SHA
    } else if bytes == BACKEND_CAPTURE.as_bytes() {
        BACKEND_CAPTURE_SHA
    } else {
        panic!("the adapter changed the independent backend SHA preimage");
    };
    parse_digest(digest).expect("fixed Node SHA-256")
}

// A deterministic test double for the required SHA-256 dependency. These tests
// exercise digest binding and byte fidelity; the desktop integration supplies
// its real sha2 implementation and owns cryptographic interoperability proof.
fn test_hash(bytes: &[u8]) -> [u8; 32] {
    let mut hash = [0_u8; 32];
    for (offset, byte) in bytes.iter().enumerate() {
        let index = offset % 32;
        hash[index] = hash[index].wrapping_mul(31).wrapping_add(*byte);
    }
    hash
}

fn scope() -> QueueScope {
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/reverse/prefix/").expect("endpoint"),
        Credential::ClientToken("oc_client_payload_fixture".to_owned()),
    )
    .expect("client");
    QueueScope::new(&client, SessionId::new("ses_payload"), DIRECTORY).expect("scope")
}

fn config_fixture() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "p".to_owned(),
        model_id: "m".to_owned(),
        agent: Some("build".to_owned()),
        variant: Some("high".to_owned()),
    }
}

fn manifest() -> QueueManifestItem {
    QueueManifestItem {
        content: "display".to_owned(),
        text: "deliver".to_owned(),
        attachment_manifest: vec![],
        context: vec![],
        context_preview: None,
        send_config: config_fixture(),
        prompt_parts: QueuePromptParts::default(),
    }
}

fn document(item: Value, usage: QueuePayloadUse) -> QueuePayloadDocument {
    QueuePayloadDocument::from_bytes(
        serde_json::to_vec(&json!({"directory":DIRECTORY,"item":item})).expect("capture JSON"),
        DIRECTORY,
        usage,
        test_hash,
    )
    .expect("typed capture")
}

fn pointer(doc: &QueuePayloadDocument) -> QueuePayloadPointer {
    let mut descriptors = Vec::new();
    for (ordinal, bytes) in doc.bytes().chunks(QUEUE_BLOB_BYTES).enumerate() {
        let mut blob_id = [0x42; 32];
        blob_id[0] = ordinal as u8;
        descriptors.push(QueueAttachmentDescriptor {
            blob_id,
            request_id: "upload_original".to_owned(),
            attachment_id: format!("payload-{ordinal}"),
            filename: format!("payload-{ordinal}.json"),
            mime_type: "application/json".to_owned(),
            size: bytes.len() as u64,
            sha256: test_hash(bytes),
        });
    }
    doc.pointer(descriptors, test_hash)
        .expect("verified pointer")
}

fn index_fixture(doc: &QueuePayloadDocument) -> QueuePayloadIndex {
    let (content, text, context_preview, entry_count, roots, legacy_ids) = match doc.item() {
        QueuePayloadItem::Manifest(item) => (
            &item.content,
            &item.text,
            item.context_preview.clone(),
            item.attachment_manifest.len()
                + parts_count(&item.prompt_parts)
                + legacy_count(&item.context),
            item.attachment_manifest
                .iter()
                .map(|file| file.attachment.blob_id)
                .collect(),
            None,
        ),
        QueuePayloadItem::Legacy(item) => (
            &item.content,
            &item.text,
            item.context_preview.clone(),
            item.attachments.len()
                + item.file_references.len()
                + usize::from(item.agent_mention.is_some())
                + legacy_count(&item.context),
            Vec::new(),
            Some(
                item.attachments
                    .iter()
                    .enumerate()
                    .map(|(ordinal, file)| match &file.id {
                        Some(id) if js_len(id) > 256 => None,
                        Some(id) => Some(id.clone()),
                        None => Some(format!(
                            "attachment-{}-{ordinal}",
                            &hex_digest(&doc.sha256())[..24]
                        )),
                    })
                    .collect(),
            ),
        ),
    };
    let context_ids = doc
        .item()
        .context_bindings()
        .into_iter()
        .enumerate()
        .map(|(ordinal, id)| id.unwrap_or_else(|| MessageId::new(format!("msg_frozen_{ordinal}"))))
        .collect();
    QueuePayloadIndex {
        directory: DIRECTORY.to_owned(),
        send_config: config_fixture(),
        preview: payload_preview_units(content, text).display(),
        context_preview: context_preview.as_ref().map(PayloadPreview::display),
        entry_count,
        context_ids,
        attachment_blob_ids: roots,
        prompt_bytes: 20,
        admission_bytes: 32,
        structured_bytes: 0,
        legacy_attachment_ids: legacy_ids,
    }
}

fn indexed_json(pointer: &QueuePayloadPointer, index: &QueuePayloadIndex) -> Value {
    let preview = PayloadPreview::from_str(&index.preview);
    let context_preview = index
        .context_preview
        .as_deref()
        .map(PayloadPreview::from_str);
    let hash = test_hash(
        &payload_binding_bytes(pointer, index, &preview, context_preview.as_ref())
            .expect("normalized binding"),
    );
    let mut value = pointer.value();
    value["payloadIndex"] = serde_json::from_slice(
        &payload_index_bytes(index, &preview, context_preview.as_ref()).expect("encoded index"),
    )
    .expect("index JSON");
    value["payloadIndexHash"] = json!(hex_digest(&hash));
    value
}

fn indexed(doc: &QueuePayloadDocument) -> QueueIndexedPayload {
    QueueIndexedPayload::from_body(
        &serde_json::to_vec(&indexed_json(&pointer(doc), &index_fixture(doc))).expect("JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("sealed pointer")
}

fn reference_fixture() -> Value {
    json!({"uri":"file:///home/synthetic/report%20one.pdf", "name":"report one.pdf",
        "description":"read the original PDF", "mention":{"start":2,"end":8,"text":"@report"}})
}

#[test]
fn explicit_references_survive_both_capture_kinds_edit_seal_restart_and_local_conversion_without_file_proof()
 {
    for manifest_kind in [false, true] {
        let mut item = json!({"content":"display", "text":"", "sendConfig":config_value(&config_fixture()),
            "promptParts":{"fileReferences":[reference_fixture(),reference_fixture()]}});
        if manifest_kind {
            item["format"] = json!("manifest-v1");
            item["attachmentManifest"] = json!([]);
        }
        let doc = document(item, QueuePayloadUse::Enqueue);
        assert!(
            doc.files_proof(test_hash).is_err(),
            "parsing references grants no sealed queue proof"
        );
        let edited = QueuePayloadDocument::from_item(
            DIRECTORY,
            doc.item(),
            QueuePayloadUse::Enqueue,
            test_hash,
        )
        .expect("lossless typed edit");
        assert_eq!(edited.item(), doc.item());
        let index = indexed(&edited);
        assert_eq!(
            index.index().entry_count,
            2,
            "duplicate reference occurrences each count"
        );
        assert!(
            index.index().attachment_blob_ids.is_empty(),
            "references add no attachment roots"
        );
        let restored = QueueIndexedPayload::from_body(
            &index.body().expect("index persistence"),
            DIRECTORY,
            test_hash,
        )
        .expect("sealed index restart");
        let sealed = restored
            .materialize(edited.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
            .expect("reference-only capture materialization");
        assert!(
            sealed
                .files_proof(test_hash)
                .expect("sealed proof")
                .files()
                .is_empty(),
            "references contribute no byte descriptors"
        );
        let local = QueueLocalCapture::from_document(sealed, vec![], test_hash)
            .expect("explicit local edit");
        assert!(local.item().files.is_empty());
        assert_eq!(local.item().prompt_parts.file_references.len(), 2);
        let local_json: Value = serde_json::from_slice(local.bytes()).expect("local JSON");
        assert_eq!(
            local_json["item"]["promptParts"]["fileReferences"],
            json!([reference_fixture(), reference_fixture()])
        );
        assert!(
            QueuePayloadDocument::from_bytes(
                local.bytes().to_vec(),
                DIRECTORY,
                QueuePayloadUse::Enqueue,
                test_hash
            )
            .is_err(),
            "local source cannot be used as queue proof"
        );
        let reopened = QueueLocalCapture::from_bytes(local.bytes().to_vec(), DIRECTORY, test_hash)
            .expect("local restart");
        assert_eq!(
            reopened.item().prompt_parts.file_references,
            local.item().prompt_parts.file_references
        );
        assert_eq!(reopened.bytes(), local.bytes());
    }
}

#[test]
fn empty_reference_defaults_preserve_existing_serialization_for_manifest_legacy_and_local_parts() {
    let parts = serde_json::to_vec(&QueuePromptParts::default()).expect("default parts");
    assert_eq!(parts, br#"{"agents":[],"skills":[],"context":[]}"#);
    for manifest_kind in [false, true] {
        let mut item = json!({"text":"deliver", "sendConfig":config_value(&config_fixture())});
        if manifest_kind {
            item["format"] = json!("manifest-v1");
            item["content"] = json!("display");
            item["attachmentManifest"] = json!([]);
        }
        let absent = document(item.clone(), QueuePayloadUse::Enqueue);
        item["promptParts"] = json!({"fileReferences":[], "agents":[], "skills":[], "context":[]});
        let empty = document(item, QueuePayloadUse::Enqueue);
        let absent = QueuePayloadDocument::from_item(
            DIRECTORY,
            absent.item(),
            QueuePayloadUse::Enqueue,
            test_hash,
        )
        .expect("absent default");
        let empty = QueuePayloadDocument::from_item(
            DIRECTORY,
            empty.item(),
            QueuePayloadUse::Enqueue,
            test_hash,
        )
        .expect("empty default");
        assert_eq!(
            absent.bytes(),
            empty.bytes(),
            "empty references add no serialized field"
        );
    }
}

#[test]
fn legacy_reference_parts_accept_empty_defaults_and_reject_official_parts_or_metadata() {
    let parts =
        json!({"fileReferences":[reference_fixture()], "agents":[], "skills":[], "context":[]});
    let mut item =
        json!({"text":"", "sendConfig":config_value(&config_fixture()), "promptParts":parts});
    let doc = document(item.clone(), QueuePayloadUse::Enqueue);
    let encoded =
        QueuePayloadDocument::from_item(DIRECTORY, doc.item(), QueuePayloadUse::Enqueue, test_hash)
            .expect("legacy reference edit");
    let body: Value = serde_json::from_slice(encoded.bytes()).expect("capture JSON");
    assert_eq!(
        body["item"]["promptParts"],
        json!({"fileReferences":[reference_fixture()]}),
        "empty defaults normalize away"
    );
    for (key, value) in [
        ("agents", json!([{"name":"build"}])),
        ("skills", json!([{"id":"skill"}])),
        ("context", json!([{"text":"official context"}])),
        ("metadata", json!({})),
        ("metadata", Value::Null),
    ] {
        item["promptParts"] = parts.clone();
        item["promptParts"][key] = value;
        assert!(
            QueuePayloadDocument::from_bytes(
                serde_json::to_vec(&json!({"directory":DIRECTORY,"item":item})).expect("JSON"),
                DIRECTORY,
                QueuePayloadUse::Enqueue,
                test_hash
            )
            .is_err(),
            "legacy references cannot bypass official-part and context boundaries"
        );
    }
}

#[test]
fn explicit_references_reject_unsafe_uris_names_mentions_and_byte_ownership_fields() {
    let mut bad = Vec::new();
    for uri in [
        "https://host/a",
        "file://host/a",
        "file://localhost/a",
        "file:/a",
        "file:///",
        "file:///a?x=1",
        "file:///a#fragment",
        "file:///a\\b",
        "file:///a\n",
        "file:///a%",
        "file:///a%xx",
    ] {
        let mut reference = reference_fixture();
        reference["uri"] = json!(uri);
        bad.push(reference);
    }
    for (key, value) in [
        ("name", json!("")),
        ("name", json!("a\n")),
        ("name", json!("é".repeat(2049))),
        ("uri", json!(format!("file:///{}", "a".repeat(65536)))),
        ("size", json!(0)),
        ("sha256", json!("00".repeat(32))),
        ("mention", json!({"start":2,"end":1,"text":"@bad"})),
        (
            "mention",
            json!({"start":0,"end":9007199254740992u64,"text":"@bad"}),
        ),
        ("description", Value::Null),
    ] {
        let mut reference = reference_fixture();
        reference[key] = value;
        bad.push(reference);
    }
    for reference in bad {
        for manifest_kind in [false, true] {
            let mut item = json!({"text":"deliver", "sendConfig":config_value(&config_fixture()), "promptParts":{"fileReferences":[reference.clone()]}});
            if manifest_kind {
                item["format"] = json!("manifest-v1");
                item["content"] = json!("");
                item["attachmentManifest"] = json!([]);
            }
            assert!(
                QueuePayloadDocument::from_bytes(
                    serde_json::to_vec(&json!({"directory":DIRECTORY,"item":item})).expect("JSON"),
                    DIRECTORY,
                    QueuePayloadUse::Enqueue,
                    test_hash
                )
                .is_err(),
                "unsafe reference must not enter either capture kind"
            );
        }
    }
    let mut item = manifest();
    item.prompt_parts.file_references = vec![
        QueueFileReference {
            uri: "file:///safe.pdf".to_owned(),
            name: "safe.pdf".to_owned(),
            description: None,
            mention: None
        };
        QUEUE_MANIFEST_ENTRIES
    ];
    assert!(
        QueuePayloadDocument::from_manifest(
            DIRECTORY,
            item.clone(),
            QueuePayloadUse::Enqueue,
            test_hash
        )
        .is_ok(),
        "reference count equality is allowed"
    );
    item.prompt_parts
        .file_references
        .push(item.prompt_parts.file_references[0].clone());
    assert!(
        QueuePayloadDocument::from_manifest(DIRECTORY, item, QueuePayloadUse::Enqueue, test_hash)
            .is_err(),
        "references share the existing total entry bound"
    );
}

#[test]
fn split_context_preview_surrogates_survive_capture_edit_index_and_materialization() {
    // A real JS slice can end after the high half of an emoji. Keep the escape
    // literal so a lossy UTF-8 replacement cannot satisfy this contract.
    let preview_json = format!(r#""{}\ud83d...""#, "a".repeat(99));
    for manifest in [false, true] {
        let fields = if manifest {
            r#""format":"manifest-v1","attachmentManifest":[],"#
        } else {
            ""
        };
        let raw = format!(
            r#"{{"directory":"{DIRECTORY}","item":{{{fields}"content":"display","text":"deliver","contextPreview":{preview_json},"sendConfig":{{"providerID":"p","modelID":"m","agent":"build","variant":"high"}}}}}}"#
        );
        let doc = QueuePayloadDocument::from_bytes(
            raw.as_bytes().to_vec(),
            DIRECTORY,
            QueuePayloadUse::Enqueue,
            test_hash,
        )
        .expect("split display preview is accepted in either capture union member");
        let context_preview = match doc.item() {
            QueuePayloadItem::Manifest(item) => item.context_preview.as_ref(),
            QueuePayloadItem::Legacy(item) => item.context_preview.as_ref(),
        }
        .expect("captured context preview");
        assert_eq!(
            context_preview.utf16()[99],
            0xd83d,
            "the split surrogate remains authoritative seal material"
        );
        assert_eq!(context_preview.utf16().len(), 103);
        let edited = QueuePayloadDocument::from_item(
            DIRECTORY,
            doc.item(),
            QueuePayloadUse::Enqueue,
            test_hash,
        )
        .expect("typed edited capture preserves the display units");
        assert!(
            String::from_utf8_lossy(edited.bytes())
                .contains(&format!("\"contextPreview\":{preview_json}"))
        );

        let pointer = pointer(&doc);
        let index = index_fixture(&doc);
        let preview = PayloadPreview::from_str(&index.preview);
        let binding = payload_binding_bytes(&pointer, &index, &preview, Some(context_preview))
            .expect("index binding bytes");
        let seal = test_hash(&binding);
        let mut bytes = pointer.body().expect("pointer JSON");
        bytes.pop();
        bytes.extend_from_slice(b",\"payloadIndex\":");
        bytes.extend_from_slice(
            &payload_index_bytes(&index, &preview, Some(context_preview))
                .expect("lossless index JSON"),
        );
        bytes.extend_from_slice(b",\"payloadIndexHash\":");
        append_json(&mut bytes, &hex_digest(&seal)).expect("index seal");
        bytes.push(b'}');
        let sealed = QueueIndexedPayload::from_body(&bytes, DIRECTORY, test_hash)
            .expect("sealed context preview");
        let restored = QueueIndexedPayload::from_body(
            &sealed.body().expect("persisted sealed index"),
            DIRECTORY,
            test_hash,
        )
        .expect("index reload preserves original UTF-16");
        assert_eq!(restored, sealed);
        assert_eq!(
            sealed.index().context_preview.as_deref(),
            Some(context_preview.display().as_str())
        );
        let materialized = restored
            .materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
            .expect("capture and index compare their exact original units");
        assert_eq!(materialized.item(), doc.item());
        let changed = String::from_utf8(bytes)
            .expect("escaped JSON is valid UTF-8")
            .replace("\\ud83d", "\\ud83e");
        assert!(
            QueueIndexedPayload::from_body(changed.as_bytes(), DIRECTORY, test_hash).is_err(),
            "changing the surrogate invalidates the old seal"
        );
    }
}

#[test]
fn edited_integer_metadata_is_checked_before_legacy_compaction() {
    let doc = document(
        json!({"text":"deliver", "context":[{"kind":"context","text":"context","metadata":{"nested":[0]}}],
        "sendConfig":config_value(&config_fixture())}),
        QueuePayloadUse::Enqueue,
    );
    for number in [
        json!(9_007_199_254_740_993_u64),
        json!(-9_007_199_254_740_993_i64),
    ] {
        let mut changed = doc.item().clone();
        let QueuePayloadItem::Legacy(item) = &mut changed else {
            panic!("legacy fixture");
        };
        let QueueLegacyContext::Context { metadata, .. } = &mut item.context[0] else {
            panic!("typed metadata fixture");
        };
        metadata.0.insert("nested".into(), json!([number]));
        assert!(
            matches!(
                QueuePayloadDocument::from_item(
                    DIRECTORY,
                    &changed,
                    QueuePayloadUse::Enqueue,
                    test_hash
                ),
                Err(QueueManifestError::InvalidInput)
            ),
            "an edited Rust integer cannot round into an accepted Number"
        );
    }
    let mut changed = doc.item().clone();
    let QueuePayloadItem::Legacy(item) = &mut changed else {
        panic!("legacy fixture");
    };
    let QueueLegacyContext::Context { metadata, .. } = &mut item.context[0] else {
        panic!("typed metadata fixture");
    };
    metadata.0.insert("nested".into(), json!([1e20_f64]));
    QueuePayloadDocument::from_item(DIRECTORY, &changed, QueuePayloadUse::Enqueue, test_hash)
        .expect(
            "existing finite JavaScript Numbers outside the safe integer range remain supported",
        );

    let mut item = manifest();
    item.prompt_parts.metadata = Some(Metadata(Map::from_iter([(
        "nested".into(),
        json!([9_007_199_254_740_993_u64]),
    )])));
    assert!(
        matches!(
            QueuePayloadDocument::from_item(
                DIRECTORY,
                &QueuePayloadItem::Manifest(item),
                QueuePayloadUse::Enqueue,
                test_hash
            ),
            Err(QueueManifestError::InvalidInput)
        ),
        "manifest edits enforce the same pre-encoding metadata boundary"
    );
}

#[test]
fn context_preview_unicode_exception_does_not_widen_authoritative_fields() {
    let config = r#""sendConfig":{"providerID":"p","modelID":"m"}"#;
    for item in [
        format!(r#"{{"text":"deliver","contextPreview":null,{config}}}"#),
        format!(r#"{{"text":"deliver","contextPreview":[],{config}}}"#),
        format!(
            r#"{{"text":"deliver","contextPreview":"{}",{config}}}"#,
            "a".repeat(104)
        ),
        format!(r#"{{"text":"\ud83d","contextPreview":"display",{config}}}"#),
        format!(
            r#"{{"text":"deliver","context":[{{"kind":"context","text":"context","metadata":{{"bad":"\ud83d"}}}}],{config}}}"#
        ),
    ] {
        let capture = format!(r#"{{"directory":"{DIRECTORY}","item":{item}}}"#);
        assert!(
            QueuePayloadDocument::from_bytes(
                capture.into_bytes(),
                DIRECTORY,
                QueuePayloadUse::Enqueue,
                test_hash
            )
            .is_err()
        );
    }
    let mut item = manifest();
    item.context_preview = Some("a".repeat(104).into());
    assert!(
        QueuePayloadDocument::from_manifest(DIRECTORY, item, QueuePayloadUse::Enqueue, test_hash)
            .is_err()
    );
}

#[test]
fn nine_mib_legacy_context_uses_one_small_pointer_and_preserves_frozen_config() {
    let context: Vec<_> = (0..9)
        .map(|_| json!({"kind":"synthetic","text":"x".repeat(1024 * 1024)}))
        .collect();
    let doc = document(
        json!({"content":"display","text":"deliver","context":context,
        "sendConfig":config_value(&config_fixture())}),
        QueuePayloadUse::Enqueue,
    );
    assert!(
        doc.bytes().len() > 9 * 1024 * 1024,
        "the fixture crosses the old 4 MiB metadata bound"
    );
    let capture = QueuePayloadEnqueue::new(
        scope().request("enqueue_original").expect("owner"),
        pointer(&doc),
        &doc,
        test_hash,
    )
    .expect("compact enqueue");
    assert!(
        capture.body().len() < 2048,
        "the request contains only a compact pointer"
    );
    let restored = QueuePayloadEnqueue::from_body(
        capture.owner().clone(),
        capture.body().to_vec(),
        &doc,
        test_hash,
    )
    .expect("exact repeat");
    assert_eq!(restored.body(), capture.body());
    let upload = doc
        .chunk_upload(
            scope().request("upload_original").expect("upload owner"),
            0,
            test_hash,
        )
        .expect("chunk capture");
    let request = upload.request().build().expect("authenticated upload");
    assert_eq!(
        request.url().path(),
        "/reverse/prefix/api/message-queue/sessions/ses_payload/requests/upload_original/attachments/payload-0"
    );
    assert_eq!(
        request.headers()["authorization"],
        "Bearer oc_client_payload_fixture"
    );
    assert_eq!(
        request.url().query_pairs().collect::<Vec<_>>(),
        vec![("directory".into(), DIRECTORY.into())]
    );
    assert_eq!(upload.bytes(), doc.bytes());
    let sealed = indexed(&doc);
    let materialized = sealed
        .materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
        .expect("verified large handoff");
    let QueuePayloadItem::Legacy(item) = materialized.item() else {
        panic!("legacy capacity is preserved");
    };
    assert_eq!(item.context.len(), 9);
    assert_eq!(item.send_config, config_fixture());
    assert_eq!(sealed.index().context_ids.len(), 9);
    assert_eq!(
        materialized.bytes(),
        doc.bytes(),
        "the original spool bytes remain immutable"
    );
}

#[test]
fn manifest_payload_retains_prompt_parts_and_hidden_roots() {
    let mut item = manifest();
    item.context.push(QueueLegacyContext::Context {
        text: "context".to_owned(),
        metadata: Metadata(Map::new()),
        instructions: Some("instructions".to_owned()),
    });
    item.prompt_parts.context.push(QueuePromptContext {
        id: Some(MessageId::new("msg_explicit")),
        text: "official context".to_owned(),
        description: Some("description".to_owned()),
        metadata: Some(Metadata(Map::from_iter([(
            "large".to_owned(),
            json!(1e20),
        )]))),
    });
    item.prompt_parts.skills.push(QueueSkill {
        id: "skill".to_owned(),
        mention: Some(QueueMention {
            start: 0,
            end: 3,
            text: "@s".to_owned(),
        }),
    });
    item.attachment_manifest.push(QueueManifestAttachment {
        attachment: QueueAttachmentDescriptor {
            blob_id: [0xaa; 32],
            request_id: "file_original".to_owned(),
            attachment_id: "file1".to_owned(),
            filename: "image.png".to_owned(),
            mime_type: "image/png".to_owned(),
            size: 20,
            sha256: [0xbb; 32],
        },
        agent_path: None,
        description: Some("image".to_owned()),
        mention: None,
    });
    let doc = QueuePayloadDocument::from_manifest(
        DIRECTORY,
        item.clone(),
        QueuePayloadUse::Enqueue,
        test_hash,
    )
    .expect("full manifest capture");
    let sealed = indexed(&doc);
    assert_eq!(sealed.index().attachment_blob_ids, vec![[0xaa; 32]]);
    let restored = sealed
        .materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
        .expect("verified full parts");
    assert_eq!(restored.item(), &QueuePayloadItem::Manifest(item));
    let mut index = index_fixture(&doc);
    index.send_config.model_id = "changed".to_owned();
    let wrong = QueueIndexedPayload::from_body(
        &serde_json::to_vec(&indexed_json(&pointer(&doc), &index)).expect("JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("valid sealed index shape");
    assert!(matches!(
        wrong.materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash),
        Err(QueueManifestError::PayloadMismatch)
    ));
    index = index_fixture(&doc);
    index.attachment_blob_ids.clear();
    let missing = QueueIndexedPayload::from_body(
        &serde_json::to_vec(&indexed_json(&pointer(&doc), &index)).expect("JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("valid empty roots schema");
    assert!(
        missing
            .materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
            .is_err()
    );
}

#[test]
fn strict_pointer_schema_rejects_index_echo_unknown_fields_and_invalid_chunks() {
    let doc = document(item_value(&manifest()), QueuePayloadUse::Enqueue);
    let pointer = pointer(&doc);
    let original = pointer.value();
    let mut mutations = Vec::new();
    for field in ["payloadIndex", "payloadIndexHash", "extra"] {
        let mut value = original.clone();
        value[field] = json!(null);
        mutations.push(value);
    }
    for (field, replacement) in [
        ("version", json!(2)),
        ("encoding", json!("json")),
        ("sha256", json!("AB".repeat(32))),
        ("totalBytes", json!(QUEUE_PAYLOAD_BYTES + 1)),
        ("chunks", json!([])),
    ] {
        let mut value = original.clone();
        value["payloadManifest"][field] = replacement;
        mutations.push(value);
    }
    for (field, replacement) in [
        ("ordinal", json!(1)),
        ("size", json!(0)),
        ("size", json!(QUEUE_BLOB_BYTES + 1)),
        ("sha256", json!("bad")),
        ("extra", json!(true)),
    ] {
        let mut value = original.clone();
        value["payloadManifest"]["chunks"][0][field] = replacement;
        mutations.push(value);
    }
    let mut duplicate = original.clone();
    let mut chunk = duplicate["payloadManifest"]["chunks"][0].clone();
    chunk["ordinal"] = json!(1);
    duplicate["payloadManifest"]["chunks"]
        .as_array_mut()
        .expect("chunks")
        .push(chunk);
    duplicate["payloadManifest"]["totalBytes"] = json!(pointer.total_bytes() * 2);
    mutations.push(duplicate);
    for value in mutations {
        assert!(
            QueuePayloadPointer::from_body(&serde_json::to_vec(&value).expect("JSON")).is_err(),
            "malformed pointer must not acquire authority"
        );
    }
    let owner = scope().request("enqueue_original").expect("owner");
    let echoed = json!({"directory":DIRECTORY,"item":indexed_json(&pointer, &index_fixture(&doc))});
    assert!(
        QueuePayloadEnqueue::from_body(
            owner,
            serde_json::to_vec(&echoed).expect("JSON"),
            &doc,
            test_hash
        )
        .is_err(),
        "a returned index is forbidden in enqueue input"
    );
}

#[test]
fn attachment_byte_proofs_require_the_original_sealed_materialization() {
    let mut item = manifest();
    item.attachment_manifest.push(QueueManifestAttachment {
        attachment: QueueAttachmentDescriptor {
            blob_id: [0xaa; 32],
            request_id: "file_original".into(),
            attachment_id: "file1".into(),
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size: 8,
            sha256: test_hash(b"original"),
        },
        agent_path: None,
        description: None,
        mention: None,
    });
    let manifest =
        QueuePayloadDocument::from_manifest(DIRECTORY, item, QueuePayloadUse::Enqueue, test_hash)
            .expect("captured manifest attachment");
    let legacy = document(
        json!({"text":"deliver", "sendConfig":config_value(&config_fixture()),
        "attachments":[{"id":"file1","filename":"notes.txt","mimeType":"text/plain",
            "dataUrl":"data:,original"}]}),
        QueuePayloadUse::Enqueue,
    );
    for doc in [manifest, legacy] {
        assert!(
            matches!(
                doc.files_proof(test_hash),
                Err(QueueManifestError::PayloadMismatch)
            ),
            "independent capture parsing grants no index-bound proof"
        );
        let sealed = indexed(&doc);
        let materialized = sealed
            .materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
            .expect("verified original capture and index");
        let proof = materialized
            .files_proof(test_hash)
            .expect("nonforgeable byte bindings");
        assert_eq!(proof.capture_sha256(), &doc.sha256());
        assert_eq!(proof.index_sha256(), &sealed.index_hash());
        let [file] = proof.files() else {
            panic!("one original file");
        };
        assert_eq!(
            file.byte_len(),
            8,
            "legacy's optional size cannot replace decoded byte length"
        );
        assert_eq!(file.sha256(), &test_hash(b"original"));
        assert_ne!(
            file.sha256(),
            &test_hash(b"tampered"),
            "same-length replacement has no original byte authority"
        );
        match materialized.item() {
            QueuePayloadItem::Manifest(_) => {
                assert_eq!(file.source(), &QueuePayloadFileSource::Manifest([0xaa; 32]))
            }
            QueuePayloadItem::Legacy(_) => {
                assert_eq!(file.source(), &QueuePayloadFileSource::Legacy(0))
            }
        }
        let edited = QueuePayloadDocument::from_item(
            DIRECTORY,
            materialized.item(),
            QueuePayloadUse::Enqueue,
            test_hash,
        )
        .expect("fresh capture after editing");
        assert!(
            matches!(
                edited.files_proof(test_hash),
                Err(QueueManifestError::PayloadMismatch)
            ),
            "reencoding cannot inherit the previous index's authority"
        );
    }
}

#[test]
fn manifest_model_paths_survive_sealed_shared_blob_materialization_local_edit_and_recapture() {
    let attachment = QueueAttachmentDescriptor {
        blob_id: [0xaa; 32],
        request_id: "file_original".into(),
        attachment_id: "file1".into(),
        filename: "safe.txt".into(),
        mime_type: "text/plain".into(),
        size: 8,
        sha256: test_hash(b"original"),
    };
    let names = [
        "/never/read/original?#%2F.txt",
        "relative/../literal\\name\t.txt",
    ];
    let mut item = manifest();
    item.attachment_manifest = names
        .iter()
        .map(|name| QueueManifestAttachment {
            attachment: attachment.clone(),
            agent_path: Some((*name).to_owned()),
            description: Some("original description".into()),
            mention: None,
        })
        .collect();
    let document = QueuePayloadDocument::from_manifest(
        DIRECTORY,
        item.clone(),
        QueuePayloadUse::Enqueue,
        test_hash,
    )
    .expect("new manifest capture");
    assert!(
        document.files_proof(test_hash).is_err(),
        "informational paths grant no byte ownership"
    );
    let original_bytes = document.bytes().to_vec();
    let mut index = index_fixture(&document);
    index.attachment_blob_ids = vec![attachment.blob_id];
    let indexed = QueueIndexedPayload::from_body(
        &serde_json::to_vec(&indexed_json(&pointer(&document), &index)).expect("sealed JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("one root for two occurrences");
    let materialized = indexed
        .materialize(original_bytes.clone(), QueuePayloadUse::Enqueue, test_hash)
        .expect("sealed capture and index");
    assert_eq!(materialized.item(), &QueuePayloadItem::Manifest(item));
    let proof = materialized
        .files_proof(test_hash)
        .expect("original sealed byte proof");
    assert_eq!(
        proof.files().len(),
        1,
        "different per-use identities do not duplicate byte roots"
    );
    assert_eq!(
        proof.files()[0].source(),
        &QueuePayloadFileSource::Manifest(attachment.blob_id)
    );
    assert_eq!(proof.files()[0].byte_len(), 8);
    assert_eq!(proof.files()[0].sha256(), &test_hash(b"original"));
    assert_eq!(proof.capture_sha256(), &document.sha256());
    assert_eq!(proof.index_sha256(), &indexed.index_hash());
    let recaptured = QueuePayloadDocument::from_item(
        DIRECTORY,
        materialized.item(),
        QueuePayloadUse::Enqueue,
        test_hash,
    )
    .expect("explicit new edited capture");
    assert_eq!(recaptured.item(), materialized.item());
    assert!(
        recaptured.files_proof(test_hash).is_err(),
        "recapture never inherits the original acknowledgement authority"
    );
    let local = QueueLocalCapture::from_document(materialized, Vec::new(), test_hash)
        .expect("explicit local conversion");
    assert_eq!(
        local
            .item()
            .files
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        names
    );
    assert_eq!(local.item().files[0].sha256, attachment.sha256);
    assert_eq!(
        document.bytes(),
        original_bytes,
        "conversion leaves original capture bytes and seals untouched"
    );
    let mut changed: Value =
        serde_json::from_slice(&original_bytes).expect("original capture JSON");
    changed["item"]["attachmentManifest"][0]["agentPath"] = json!("/changed/identity.txt");
    assert!(
        indexed
            .materialize(
                serde_json::to_vec(&changed).expect("changed capture"),
                QueuePayloadUse::Enqueue,
                test_hash
            )
            .is_err(),
        "the original capture seal also binds informational identity"
    );
}

#[test]
fn capture_and_chunk_limits_accept_equality_and_refuse_one_byte_over() {
    let doc = document(item_value(&manifest()), QueuePayloadUse::Enqueue);
    let template = pointer(&doc).chunks()[0].attachment.clone();
    let mut chunks = Vec::new();
    for (ordinal, size) in [QUEUE_BLOB_BYTES, QUEUE_BLOB_BYTES, 10 * 1024 * 1024]
        .into_iter()
        .enumerate()
    {
        let mut attachment = template.clone();
        attachment.blob_id[0] = ordinal as u8;
        attachment.size = size as u64;
        chunks.push(QueuePayloadChunk {
            ordinal: ordinal as u64,
            attachment,
        });
    }
    assert!(
        QueuePayloadPointer::new(QUEUE_PAYLOAD_BYTES as u64, [0; 32], chunks.clone()).is_ok(),
        "50 MiB captures and 20 MiB chunks accept equality"
    );
    assert!(matches!(
        QueuePayloadPointer::new(QUEUE_PAYLOAD_BYTES as u64 + 1, [0; 32], chunks.clone()),
        Err(QueueManifestError::PayloadTooLarge)
    ));
    chunks[0].attachment.size += 1;
    assert!(
        QueuePayloadPointer::new(QUEUE_PAYLOAD_BYTES as u64, [0; 32], chunks).is_err(),
        "a chunk cannot cross 20 MiB"
    );
    let mut short = pointer(&doc).value();
    short["payloadManifest"]["totalBytes"] = json!(doc.bytes().len() + 1);
    assert!(
        QueuePayloadPointer::from_body(&serde_json::to_vec(&short).expect("JSON")).is_err(),
        "chunk lengths must sum exactly before reads"
    );
}

#[test]
fn index_digest_directory_and_whole_chunk_digests_are_all_checked() {
    let doc = document(item_value(&manifest()), QueuePayloadUse::Enqueue);
    let original = indexed_json(&pointer(&doc), &index_fixture(&doc));
    let bytes = serde_json::to_vec(&original).expect("JSON");
    assert!(matches!(
        QueueIndexedPayload::from_body(&bytes, "/other", test_hash),
        Err(QueueManifestError::ReceiptMismatch)
    ));
    for field in ["preview", "promptBytes", "sendConfig", "contextIds"] {
        let mut value = original.clone();
        value["payloadIndex"][field] = match field {
            "preview" => json!("changed"),
            "promptBytes" => json!(21),
            "sendConfig" => json!({"providerID":"p","modelID":"other"}),
            _ => json!(["msg_changed"]),
        };
        assert!(
            QueueIndexedPayload::from_body(
                &serde_json::to_vec(&value).expect("JSON"),
                DIRECTORY,
                test_hash
            )
            .is_err(),
            "an altered index cannot retain its old hash"
        );
    }
    let sealed = indexed(&doc);
    let mut corrupt = doc.bytes().to_vec();
    corrupt[0] = b'[';
    assert!(matches!(
        sealed.materialize(corrupt, QueuePayloadUse::Enqueue, test_hash),
        Err(QueueManifestError::PayloadMismatch)
    ));
    let mut changed = pointer(&doc);
    changed.sha256 = [0x11; 32];
    let value = indexed_json(&changed, &index_fixture(&doc));
    let sealed = QueueIndexedPayload::from_body(
        &serde_json::to_vec(&value).expect("JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("re-sealed pointer");
    assert!(matches!(
        sealed.materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash),
        Err(QueueManifestError::PayloadMismatch)
    ));
}

#[test]
fn strict_typed_document_rejects_invalid_utf8_bom_shapes_and_owners() {
    let raw = json!({"directory":DIRECTORY,"item":item_value(&manifest())});
    let original = serde_json::to_vec(&raw).expect("JSON");
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(&original);
    for bytes in [
        vec![0xff],
        bom,
        b"null".to_vec(),
        b"[]".to_vec(),
        [original.clone(), b" true".to_vec()].concat(),
    ] {
        assert!(
            QueuePayloadDocument::from_bytes(bytes, DIRECTORY, QueuePayloadUse::Enqueue, test_hash)
                .is_err()
        );
    }
    assert!(
        QueuePayloadDocument::from_bytes(original, "/wrong", QueuePayloadUse::Enqueue, test_hash)
            .is_err()
    );
    for path in [
        ["extra", ""],
        ["item", "goal"],
        ["item", "id"],
        ["item", "attachments"],
        ["item", "payloadIndex"],
    ] {
        let mut value = raw.clone();
        if path[1].is_empty() {
            value[path[0]] = json!(null);
        } else {
            value[path[0]][path[1]] = json!(null);
        }
        assert!(
            QueuePayloadDocument::from_bytes(
                serde_json::to_vec(&value).expect("JSON"),
                DIRECTORY,
                QueuePayloadUse::Enqueue,
                test_hash
            )
            .is_err()
        );
    }
    let empty = QueueManifestItem {
        content: String::new(),
        text: String::new(),
        ..manifest()
    };
    assert!(
        QueuePayloadDocument::from_manifest(
            DIRECTORY,
            empty.clone(),
            QueuePayloadUse::Enqueue,
            test_hash
        )
        .is_err()
    );
    assert!(
        QueuePayloadDocument::from_manifest(DIRECTORY, empty, QueuePayloadUse::Composer, test_hash)
            .is_ok()
    );
}

#[test]
fn legacy_identity_bindings_restore_short_long_and_missing_ids_before_edit() {
    let long_id = "i".repeat(257);
    let doc = document(
        json!({"content":"display","text":"deliver","sendConfig":config_value(&config_fixture()),
        "attachments":[
            {"filename":"one","mimeType":"text/plain","agentPath":"/metadata/only/original one.txt","dataUrl":"data:;base64,YQ=="},
            {"id":long_id,"filename":"two","mimeType":"text/plain","dataUrl":"data:,b%2Bc","source":"server","serverPath":"/metadata/only"}
        ]}),
        QueuePayloadUse::Enqueue,
    );
    let mut index = index_fixture(&doc);
    index.legacy_attachment_ids = Some(vec![Some("original-short-id".to_owned()), None]);
    let sealed = QueueIndexedPayload::from_body(
        &serde_json::to_vec(&indexed_json(&pointer(&doc), &index)).expect("JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("bindings");
    let materialized = sealed
        .materialize(doc.bytes().to_vec(), QueuePayloadUse::Enqueue, test_hash)
        .expect("restored file identities");
    let QueuePayloadItem::Legacy(item) = materialized.item() else {
        panic!("typed legacy capture");
    };
    assert_eq!(item.attachments[0].id.as_deref(), Some("original-short-id"));
    assert_eq!(item.attachments[1].id.as_deref(), Some(long_id.as_str()));
    assert_eq!(item.attachments[0].bytes().expect("base64 bytes"), b"a");
    assert_eq!(
        item.attachments[0].agent_path.as_deref(),
        Some("/metadata/only/original one.txt")
    );
    assert_eq!(item.attachments[1].bytes().expect("percent bytes"), b"b+c");
    let edited = QueuePayloadDocument::from_item(
        DIRECTORY,
        materialized.item(),
        QueuePayloadUse::Enqueue,
        test_hash,
    )
    .expect("fresh edited capture");
    let capture: Value = serde_json::from_slice(edited.bytes()).expect("typed JSON");
    assert_eq!(capture["item"]["attachments"][0]["id"], "original-short-id");
    assert_eq!(
        capture["item"]["attachments"][0]["agentPath"],
        "/metadata/only/original one.txt"
    );
    assert!(capture["item"]["attachments"][0].get("size").is_none());
    assert!(capture["item"]["attachments"][0].get("source").is_none());
    assert_eq!(capture["item"]["attachments"][1]["source"], "server");
    assert!(capture["item"].get("payloadIndex").is_none());
    for data in [
        "file:///must-not-read",
        "data:;base64,YR==",
        "data:;base64,YQ=",
        "data:;base64,YQ==YQ==",
        "data:,%ff",
        "data:,%z0",
    ] {
        assert!(
            inline_payload_bytes(data).is_err(),
            "invalid inline bytes cannot be spooled"
        );
    }
}

#[test]
fn prefix_and_take_keep_exact_owner_order_frozen_contexts_and_compact_ownership() {
    let mut item = manifest();
    item.prompt_parts.context.push(QueuePromptContext {
        id: Some(MessageId::new("msg_item_context")),
        text: "context".to_owned(),
        description: None,
        metadata: None,
    });
    let queued =
        QueuePayloadDocument::from_manifest(DIRECTORY, item, QueuePayloadUse::Enqueue, test_hash)
            .expect("queued capture");
    let composer = QueuePayloadDocument::from_manifest(
        DIRECTORY,
        manifest(),
        QueuePayloadUse::Composer,
        test_hash,
    )
    .expect("composer capture");
    let capture = QueuePayloadPrefixCapture::new(
        scope(),
        QueuePayloadPrefixInput {
            id: MessageId::new("msg_reserved"),
            item_ids: vec![QueuedItemId::new("queued1")],
            composer: pointer(&composer),
            steer: false,
        },
        test_hash,
    )
    .expect("prefix capture");
    let mut reservation: Value = serde_json::from_slice(capture.body()).expect("prefix JSON");
    reservation["composer"] = indexed_json(&pointer(&composer), &index_fixture(&composer));
    reservation["delivery"] = Value::Null;
    let mut queued_value = indexed_json(&pointer(&queued), &index_fixture(&queued));
    queued_value["id"] = json!("queued1");
    queued_value["createdAt"] = json!(17);
    queued_value["requestId"] = json!("source_enqueue");
    let reply = json!({"revision":2,"reservation":reservation,"items":[queued_value],"contextIds":["msg_item_context"]});
    let parsed = capture
        .parse_reply(&serde_json::to_vec(&reply).expect("JSON"))
        .expect("compact reservation");
    assert_eq!(
        parsed.items[0].request_id.as_deref(),
        Some("source_enqueue")
    );
    assert_eq!(parsed.composer.index().send_config, config_fixture());
    let restored =
        QueuePayloadPrefixCapture::from_body(scope(), capture.body().to_vec(), test_hash)
            .expect("exact request repeat");
    assert_eq!(restored.body(), capture.body());
    for field in ["directory", "id", "itemIds"] {
        let mut wrong = reply.clone();
        wrong["reservation"][field] = match field {
            "directory" => json!("/other"),
            "id" => json!("msg_other"),
            _ => json!(["other"]),
        };
        assert!(
            capture
                .parse_reply(&serde_json::to_vec(&wrong).expect("JSON"))
                .is_err()
        );
    }
    let take = QueuePayloadTake::new(
        scope().request("take_original").expect("owner"),
        vec![QueuedItemId::new("queued1")],
        test_hash,
    )
    .expect("durable take");
    let mut transfer = json!({"receipt":{"version":1,"kind":"take","requestId":"take_original","sessionId":"ses_payload","directory":DIRECTORY,
        "requestHash":"aa".repeat(32),"acceptedAt":1,"itemIds":["queued1"]},"state":"transferring","revision":3,"items":reply["items"]});
    let taken = take
        .parse_reply(&serde_json::to_vec(&transfer).expect("JSON"))
        .expect("compact transfer");
    assert!(matches!(
        taken.state,
        QueuePayloadTakeState::Transferring { .. }
    ));
    let ack = QueueAcknowledge::new(take.owner().clone(), taken.receipt)
        .expect("explicit local ownership ack");
    assert_eq!(
        serde_json::from_slice::<Value>(ack.body()).expect("JSON"),
        json!({"directory":DIRECTORY})
    );
    transfer["state"] = json!("acknowledged");
    transfer.as_object_mut().expect("reply").remove("items");
    assert!(matches!(
        take.parse_reply(&serde_json::to_vec(&transfer).expect("JSON"))
            .expect("acknowledged")
            .state,
        QueuePayloadTakeState::Acknowledged
    ));
}

#[test]
fn backend_index_hash_preimage_uses_schema_order_independent_of_wire_key_order() {
    fn inspect_preimage(bytes: &[u8]) -> [u8; 32] {
        let expected = format!(
            "{{\"payloadManifest\":{{\"version\":1,\"encoding\":\"utf-8-json\",\"totalBytes\":1,\"sha256\":\"{}\",\"chunks\":[{{\"attachmentId\":\"a\",\"filename\":\"p.json\",\"mimeType\":\"application/json\",\"size\":1,\"sha256\":\"{}\",\"blobId\":\"{}\",",
            "00".repeat(32),
            "22".repeat(32),
            "11".repeat(32)
        );
        assert!(
            std::str::from_utf8(bytes)
                .expect("UTF-8 JSON")
                .starts_with(expected.as_str())
        );
        let expected_end = "\"requestId\":\"upload_id\",\"ordinal\":0}]},\"payloadIndex\":{\"directory\":\"/d\",\"sendConfig\":{\"providerID\":\"p\",\"modelID\":\"m\"},\"preview\":\"\",\"entryCount\":0,\"contextIds\":[],\"attachmentBlobIds\":[],\"promptBytes\":0,\"admissionBytes\":0,\"structuredBytes\":0}}";
        assert!(
            std::str::from_utf8(bytes)
                .expect("UTF-8 JSON")
                .ends_with(expected_end),
            "schema-normalized order is part of the backend SHA contract"
        );
        [0x55; 32]
    }
    let value = json!({"payloadIndexHash":"55".repeat(32),"payloadIndex":{"structuredBytes":0,"admissionBytes":0,"promptBytes":0,"attachmentBlobIds":[],"contextIds":[],"entryCount":0,"preview":"","sendConfig":{"modelID":"m","providerID":"p"},"directory":"/d"},
        "payloadManifest":{"chunks":[{"ordinal":0,"requestId":"upload_id","blobId":"11".repeat(32),"sha256":"22".repeat(32),"size":1,"mimeType":"application/json","filename":"p.json","attachmentId":"a"}],"sha256":"00".repeat(32),"totalBytes":1,"encoding":"utf-8-json","version":1},"format":"payload-v1"});
    QueueIndexedPayload::from_body(
        &serde_json::to_vec(&value).expect("JSON"),
        "/d",
        inspect_preimage,
    )
    .expect("schema-normalized binding");
}

#[test]
fn backend_split_surrogate_preview_preserves_the_independent_seal_and_full_capture() {
    assert_eq!(
        BACKEND_CAPTURE.len(),
        313,
        "the fixed Node capture bytes remain unchanged"
    );
    let sealed = QueueIndexedPayload::from_body(
        BACKEND_INDEXED.as_bytes(),
        DIRECTORY,
        backend_fixture_digest,
    )
    .expect("backend UTF-16 preview is a valid compact handoff");
    assert_eq!(hex_digest(&sealed.index_hash()), BACKEND_INDEX_SHA);
    assert_eq!(
        sealed.index().preview,
        format!("{}\u{fffd}...", "a".repeat(99)),
        "display replacement has no sealed-byte authority"
    );
    let persisted = sealed.body().expect("lossless compact persistence");
    let text = std::str::from_utf8(&persisted).expect("JSON remains valid UTF-8");
    assert!(
        text.contains(r"\ud83d..."),
        "persistence keeps the original lone UTF-16 unit"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "display replacement must never enter the sealed index"
    );
    assert_eq!(
        QueueIndexedPayload::from_body(&persisted, DIRECTORY, backend_fixture_digest)
            .expect("restored seal"),
        sealed
    );
    let materialized = sealed
        .materialize(
            BACKEND_CAPTURE.as_bytes().to_vec(),
            QueuePayloadUse::Enqueue,
            backend_fixture_digest,
        )
        .expect("the exact independent chunk and capture digests match");
    let QueuePayloadItem::Manifest(item) = materialized.item() else {
        panic!("backend manifest capture");
    };
    assert_eq!(
        item.content,
        format!("{}😀", "a".repeat(99)),
        "display slicing cannot truncate editable content"
    );
    assert_eq!(item.send_config, config_fixture());
}

#[test]
fn backend_surrogate_fixture_survives_both_take_and_prefix_reply_decoding() {
    let taken = format!(
        "{},\"id\":\"queued1\",\"createdAt\":17}}",
        &BACKEND_INDEXED[..BACKEND_INDEXED.len() - 1]
    );
    let take = QueuePayloadTake::new(
        scope().request("take_original").expect("owner"),
        vec![QueuedItemId::new("queued1")],
        backend_fixture_digest,
    )
    .expect("take capture");
    let reply = format!(
        r#"{{"receipt":{{"version":1,"kind":"take","requestId":"take_original","sessionId":"ses_payload","directory":"/home/synthetic/large context","requestHash":"{}","acceptedAt":1,"itemIds":["queued1"]}},"state":"transferring","revision":3,"items":[{taken}]}}"#,
        "aa".repeat(32)
    );
    assert!(matches!(
        take.parse_reply(reply.as_bytes())
            .expect("backend take preview")
            .state,
        QueuePayloadTakeState::Transferring { .. }
    ));
    let sealed = QueueIndexedPayload::from_body(
        BACKEND_INDEXED.as_bytes(),
        DIRECTORY,
        backend_fixture_digest,
    )
    .expect("composer pointer");
    let prefix = QueuePayloadPrefixCapture::new(
        scope(),
        QueuePayloadPrefixInput {
            id: MessageId::new("msg_reserved"),
            item_ids: vec![QueuedItemId::new("queued1")],
            composer: sealed.pointer().clone(),
            steer: false,
        },
        backend_fixture_digest,
    )
    .expect("prefix capture");
    let reply = format!(
        r#"{{"revision":2,"reservation":{{"format":"payload-v1","directory":"/home/synthetic/large context","id":"msg_reserved","itemIds":["queued1"],"composer":{BACKEND_INDEXED},"delivery":null}},"items":[{taken}],"contextIds":[]}}"#
    );
    let parsed = prefix
        .parse_reply(reply.as_bytes())
        .expect("both composer and item retain surrogate previews");
    assert_eq!(parsed.composer, sealed);
    assert_eq!(parsed.items[0].payload, sealed);
}

#[test]
fn preview_byte_decoder_keeps_the_strict_json_string_boundary() {
    for bytes in [b"[97]".as_slice(), b"\"raw\ncontrol\"", b"\"\xed\xa0\xbd\""] {
        assert!(
            decode::<PayloadPreview>(bytes).is_err(),
            "arrays, raw controls and malformed UTF-8 remain forbidden"
        );
    }
    assert!(
        decode::<PayloadPreview>(br#""\ud83d""#).is_ok(),
        "a JSON escape may contain the backend's lone surrogate"
    );
    let too_long = format!("\"{}\\ud83d\"", "a".repeat(103));
    assert!(
        decode::<PayloadPreview>(too_long.as_bytes()).is_err(),
        "the 103 UTF-16-unit bound remains exact"
    );
}

#[test]
fn legacy_reencoding_matches_independent_backend_number_spelling_and_default_omission() {
    let numbers: Value =
        serde_json::from_str(BACKEND_NUMERIC_INPUT).expect("fixed backend Number inputs");
    assert_eq!(
        encode_legacy_payload(&numbers).expect("compact number encoding"),
        BACKEND_NUMERIC_COMPACT.as_bytes()
    );
    let captured = QueuePayloadDocument::from_bytes(
        BACKEND_LEGACY_CAPTURE.as_bytes().to_vec(),
        DIRECTORY,
        QueuePayloadUse::Enqueue,
        test_hash,
    )
    .expect("backend compact legacy capture");
    let encoded = QueuePayloadDocument::from_item(
        DIRECTORY,
        captured.item(),
        QueuePayloadUse::Enqueue,
        test_hash,
    )
    .expect("compact legacy edit encoding");
    assert_eq!(
        encoded.bytes().len(),
        BACKEND_LEGACY_CAPTURE.len(),
        "normalizing Number values cannot amplify the capture"
    );
    let raw: Value = serde_json::from_slice(encoded.bytes()).expect("encoded capture");
    for field in ["content", "text", "attachments"] {
        assert!(raw["item"].get(field).is_none(), "omit redundant {field}");
    }
    assert!(
        std::str::from_utf8(encoded.bytes())
            .expect("UTF-8")
            .contains(BACKEND_NUMERIC_COMPACT)
    );
    assert_eq!(
        encoded.item(),
        captured.item(),
        "all semantic content and captured configuration survive compaction"
    );
    let manifest = QueuePayloadItem::Manifest(manifest());
    let unchanged =
        QueuePayloadDocument::from_item(DIRECTORY, &manifest, QueuePayloadUse::Enqueue, test_hash)
            .expect("existing manifest encoding");
    let raw: Value = serde_json::from_slice(unchanged.bytes()).expect("manifest JSON");
    assert!(raw["item"].get("content").is_some());
    assert!(raw["item"].get("attachmentManifest").is_some());
}

#[test]
fn shortening_a_fifty_mib_compact_legacy_capture_does_not_reintroduce_defaults_or_numeric_amplification()
 {
    // Expand only the context text of the fixed backend capture. Its typed shape
    // and independently generated numeric spellings stay unchanged.
    let (prefix, suffix) = BACKEND_LEGACY_CAPTURE
        .split_once("\"padding\"")
        .expect("backend text marker");
    let length = QUEUE_PAYLOAD_BYTES - prefix.len() - suffix.len() - 2;
    let mut bytes = Vec::with_capacity(QUEUE_PAYLOAD_BYTES);
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.push(b'"');
    bytes.resize(bytes.len() + length, b'x');
    bytes.push(b'"');
    bytes.extend_from_slice(suffix.as_bytes());
    assert_eq!(bytes.len(), QUEUE_PAYLOAD_BYTES);
    let captured =
        QueuePayloadDocument::from_bytes(bytes, DIRECTORY, QueuePayloadUse::Enqueue, test_hash)
            .expect("original 50 MiB capture accepts equality");
    let mut edited = captured.item().clone();
    drop(captured);
    let QueuePayloadItem::Legacy(item) = &mut edited else {
        panic!("legacy item");
    };
    let QueueLegacyContext::Context { text, .. } = &mut item.context[0] else {
        panic!("captured context");
    };
    text.pop();
    let encoded =
        QueuePayloadDocument::from_item(DIRECTORY, &edited, QueuePayloadUse::Enqueue, test_hash)
            .expect("a shortened edit remains within the original capture budget");
    assert_eq!(encoded.bytes().len(), QUEUE_PAYLOAD_BYTES - 1);
    assert_eq!(encoded.item(), &edited);
    drop(encoded);
    let QueuePayloadItem::Legacy(item) = &mut edited else {
        panic!("legacy item");
    };
    let QueueLegacyContext::Context { text, .. } = &mut item.context[0] else {
        panic!("captured context");
    };
    text.push_str("xx");
    assert!(
        matches!(
            QueuePayloadDocument::from_item(
                DIRECTORY,
                &edited,
                QueuePayloadUse::Enqueue,
                test_hash
            ),
            Err(QueueManifestError::PayloadTooLarge)
        ),
        "an actual one-byte overrun still fails before ownership"
    );
}
