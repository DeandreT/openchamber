//! Encoder and scripted dispatch definitions. No real runtime or provider is used.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::credential::Credential;
use crate::endpoint::Endpoint;
use crate::prompt_parts::Metadata;
use crate::queue_manifest::{
    QueueAgent, QueueAttachmentDescriptor, QueueFileReference, QueueLocalFile, QueuePayloadUse,
    QueuePromptContext, QueuePromptParts, QueueSkill,
};

use super::*;

const DIRECTORY: &str = "/home/synthetic/queue";
const SESSION: &str = "ses_frozen_queue";
const MESSAGE: &str = "msg_frozen_original";

// Test double only. Cryptographic interoperability remains the desktop's real
// SHA-256 callback's responsibility, as in queue_payload_tests.rs.
fn test_hash(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0u8; 32];
    for (index, byte) in bytes.iter().enumerate() {
        result[index % 32] = result[index % 32].wrapping_mul(31).wrapping_add(*byte);
    }
    result
}

fn hex(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn client() -> ServerClient {
    ServerClient::new(
        Endpoint::parse("https://example.invalid/prefix/").expect("endpoint"),
        Credential::ClientToken("frozen-test-credential".to_owned()),
    )
    .expect("client")
}

fn config() -> Value {
    json!({"providerID":"provider", "modelID":"model", "agent":"build", "variant":"high"})
}

fn manifest() -> Value {
    json!({"format":"manifest-v1", "content":"display only", "text":"delivered first",
        "attachmentManifest":[], "sendConfig":config()})
}

// These field orders are the backend's schema-normalized hash preimage, not
// serde_json::Value's sorted-object serialization.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Chunk {
    attachment_id: String,
    filename: String,
    mime_type: String,
    size: u64,
    sha256: String,
    blob_id: String,
    request_id: String,
    ordinal: usize,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u8,
    encoding: &'static str,
    total_bytes: usize,
    sha256: String,
    chunks: Vec<Chunk>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    directory: &'static str,
    send_config: Configuration,
    preview: &'static str,
    entry_count: usize,
    context_ids: Vec<String>,
    attachment_blob_ids: Vec<String>,
    prompt_bytes: u64,
    admission_bytes: u64,
    structured_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_attachment_ids: Option<Vec<Option<String>>>,
}

fn sealed(
    item: Value,
    ids: &[&str],
    structured_bytes: u64,
) -> (QueuePayloadDocument, QueueIndexedPayload) {
    let bytes = serde_json::to_vec(&json!({"directory":DIRECTORY,"item":item})).expect("capture");
    let document =
        QueuePayloadDocument::from_bytes(bytes, DIRECTORY, QueuePayloadUse::Composer, test_hash)
            .expect("typed capture");
    let mut chunks = Vec::new();
    let mut descriptors = Vec::new();
    for (ordinal, bytes) in document.bytes().chunks(QUEUE_BLOB_BYTES).enumerate() {
        let mut blob = [0x42; 32];
        blob[0] = ordinal as u8;
        let descriptor = QueueAttachmentDescriptor {
            blob_id: blob,
            request_id: "upload_original".to_owned(),
            attachment_id: format!("payload-{ordinal}"),
            filename: format!("payload-{ordinal}.json"),
            mime_type: "application/json".to_owned(),
            size: bytes.len() as u64,
            sha256: test_hash(bytes),
        };
        chunks.push(Chunk {
            attachment_id: descriptor.attachment_id.clone(),
            filename: descriptor.filename.clone(),
            mime_type: descriptor.mime_type.clone(),
            size: descriptor.size,
            sha256: hex(&descriptor.sha256),
            blob_id: hex(&blob),
            request_id: descriptor.request_id.clone(),
            ordinal,
        });
        descriptors.push(descriptor);
    }
    let pointer = document.pointer(descriptors, test_hash).expect("pointer");
    let (entries, roots, legacy) = match document.item() {
        QueuePayloadItem::Manifest(item) => {
            let mut roots = Vec::new();
            for file in &item.attachment_manifest {
                let id = hex(&file.attachment.blob_id);
                if !roots.contains(&id) {
                    roots.push(id);
                }
            }
            (
                item.attachment_manifest.len()
                    + item.prompt_parts.file_references.len()
                    + ids.len()
                    + item.prompt_parts.agents.len()
                    + item.prompt_parts.skills.len(),
                roots,
                None,
            )
        }
        QueuePayloadItem::Legacy(item) => (
            item.attachments.len()
                + item.file_references.len()
                + ids.len()
                + usize::from(item.agent_mention.is_some()),
            Vec::new(),
            Some(
                item.attachments
                    .iter()
                    .enumerate()
                    .map(|(ordinal, file)| {
                        Some(
                            file.id
                                .clone()
                                .unwrap_or_else(|| format!("original-attachment-{ordinal}")),
                        )
                    })
                    .collect(),
            ),
        ),
    };
    let manifest = Manifest {
        version: 1,
        encoding: "utf-8-json",
        total_bytes: document.bytes().len(),
        sha256: hex(&document.sha256()),
        chunks,
    };
    let index = Index {
        directory: DIRECTORY,
        send_config: Configuration::capture(document.item().send_config()),
        preview: "display only",
        entry_count: entries,
        context_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        attachment_blob_ids: roots,
        prompt_bytes: 1,
        admission_bytes: 1,
        structured_bytes,
        legacy_attachment_ids: legacy,
    };
    let binding = format!(
        "{{\"payloadManifest\":{},\"payloadIndex\":{}}}",
        serde_json::to_string(&manifest).expect("manifest"),
        serde_json::to_string(&index).expect("index")
    );
    let mut value: Value =
        serde_json::from_slice(&pointer.body().expect("pointer body")).expect("pointer JSON");
    value["payloadIndex"] = serde_json::to_value(index).expect("index JSON");
    value["payloadIndexHash"] = json!(hex(&test_hash(binding.as_bytes())));
    let indexed = QueueIndexedPayload::from_body(
        &serde_json::to_vec(&value).expect("indexed JSON"),
        DIRECTORY,
        test_hash,
    )
    .expect("sealed index");
    let materialized = indexed
        .materialize(
            document.bytes().to_vec(),
            QueuePayloadUse::Composer,
            test_hash,
        )
        .expect("sealed materialization");
    (materialized, indexed)
}

fn capture(
    client: &ServerClient,
    sources: &[QueueFrozenItem<'_>],
    selected: usize,
) -> Result<QueueFrozenSend, QueueFrozenSendError> {
    QueueFrozenSend::capture(
        client,
        QueueFrozenSendInput {
            session: &SessionId::new(SESSION),
            directory: DIRECTORY,
            message_id: &MessageId::new(MESSAGE),
            items: sources,
            selected_input: selected,
            delivery: Some(Delivery::Steer),
        },
        test_hash,
    )
}

fn source<'a>(
    document: &'a QueuePayloadDocument,
    index: &'a QueueIndexedPayload,
    files: &'a [QueuePreparedFile],
) -> QueueFrozenItem<'a> {
    QueueFrozenItem {
        document,
        indexed_payload: index,
        files,
    }
}

fn restore(
    client: &ServerClient,
    send: &QueueFrozenSend,
    bytes: Vec<u8>,
    digest: [u8; 32],
) -> Result<QueueFrozenSend, QueueFrozenSendError> {
    QueueFrozenSend::from_bytes(
        client,
        QueueFrozenSendOwner {
            session: &SessionId::new(SESSION),
            directory: DIRECTORY,
            message_id: &MessageId::new(MESSAGE),
            configuration: &send.configuration(),
        },
        bytes,
        digest,
        test_hash,
    )
}

#[test]
fn rich_capture_preserves_every_wire_field_and_original_identity() {
    let blob = [3; 32];
    let mut item = manifest();
    item["attachmentManifest"] = json!([{"blobId":hex(&blob),"requestId":"file_original","attachmentId":"file",
        "filename":"notes.txt","mimeType":"text/plain","size":2,"sha256":hex(&test_hash(b"hi")),
        "description":"file description", "mention":{"start":4294967297u64,"end":4294967300u64,"text":"@notes"}}]);
    item["context"] = json!([{"kind":"context","text":"context body","metadata":{"quoted":{"x":[1,true,null]}},"instructions":"read first"}]);
    item["promptParts"] = json!({"agents":[{"name":"plan","mention":{"start":0,"end":5,"text":"@plan"}}],
        "fileReferences":[{"uri":"file:///home/synthetic/report.pdf","name":"report.pdf","description":"PDF reference","mention":{"start":1,"end":7,"text":"@report"}}],
        "skills":[{"id":"skill_original","mention":{"start":6,"end":12,"text":"$skill"}}],
        "metadata":{"nested":{"array":[1,true,null]}},
        "context":[{"id":"msg_official_original","text":"rich context","description":"context description","metadata":{"source":"selection"}}]});
    let (doc, index) = sealed(
        item,
        &[
            "msg_instruction_original",
            "msg_legacy_original",
            "msg_official_original",
        ],
        500,
    );
    let bindings = [QueuePreparedFile::new(
        QueuePayloadFileSource::Manifest(blob),
        2,
        test_hash(b"hi"),
        "file:///tmp/owned/notes.txt",
    )
    .expect("binding")];
    let send = capture(&client(), &[source(&doc, &index, &bindings)], 0).expect("complete send");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("prompt JSON");
    assert_eq!(
        body,
        json!({"id":MESSAGE,"text":"delivered first","delivery":"steer",
        "files":[{"uri":"file:///tmp/owned/notes.txt","name":"notes.txt","description":"file description",
            "mention":{"start":4294967297u64,"end":4294967300u64,"text":"@notes"}},
            {"uri":"file:///home/synthetic/report.pdf","name":"report.pdf","description":"PDF reference","mention":{"start":1,"end":7,"text":"@report"}}],
        "agents":[{"name":"plan","mention":{"start":0,"end":5,"text":"@plan"}}],
        "skills":[{"id":"skill_original","mention":{"start":6,"end":12,"text":"$skill"}}],
        "metadata":{"nested":{"array":[1,true,null]}}})
    );
    let context: Vec<Value> = (0..3)
        .map(|index| {
            serde_json::from_slice(send.context_body(index).expect("context")).expect("JSON")
        })
        .collect();
    assert_eq!(
        context
            .iter()
            .map(|body| body["text"].as_str().expect("text"))
            .collect::<Vec<_>>(),
        ["read first", "context body", "rich context"]
    );
    assert_eq!(context[2]["description"], "context description");
    assert_eq!(
        context[1]["metadata"]["quoted"]["x"],
        json!([1, true, null])
    );
    assert!(
        context
            .iter()
            .all(|body| body["resume"] == false && body["delivery"] == "steer")
    );
    let reopened =
        restore(&client(), &send, send.bytes().to_vec(), send.sha256()).expect("reconstruction");
    assert_eq!(
        reopened.bytes(),
        send.bytes(),
        "frame bytes survive restart"
    );
    assert_eq!(
        reopened.prompt_body(),
        send.prompt_body(),
        "request bytes survive restart"
    );
    assert_eq!(reopened.context_ids(), send.context_ids());
    for private in [
        "display only",
        "delivered first",
        "file description",
        "context description",
        "notes.txt",
        "frozen-test-credential",
    ] {
        assert!(
            !format!("{send:?}").contains(private),
            "Debug redacts {private}"
        );
    }
}

#[test]
fn text_only_body_matches_the_existing_pinned_sdk_byte_oracle() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/transport.json"))
        .expect("SDK fixture");
    let request = fixture["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .find(|request| request["name"] == "submit text prompt")
        .expect("pinned prompt request");
    let expected: Value =
        serde_json::from_str(request["body"].as_str().expect("request body")).expect("JSON");
    let mut item = manifest();
    item["text"] = expected["text"].clone();
    let (doc, index) = sealed(item, &[], 100);
    let sources = [source(&doc, &index, &[])];
    let send = QueueFrozenSend::capture(
        &client(),
        QueueFrozenSendInput {
            session: &SessionId::new(SESSION),
            directory: DIRECTORY,
            message_id: &MessageId::new(expected["id"].as_str().expect("prompt ID")),
            items: &sources,
            selected_input: 0,
            delivery: Some(Delivery::Queue),
        },
        test_hash,
    )
    .expect("SDK-shaped capture");
    assert_eq!(
        send.prompt_body(),
        request["body"].as_str().expect("request body").as_bytes()
    );
}

#[test]
fn legacy_inline_bytes_are_bound_to_staging_and_agent_path_only_names_the_file() {
    let (doc, index) = sealed(
        json!({"content":"display only","text":"delivered first","agentMention":"plan","sendConfig":config(),
        "promptParts":{"fileReferences":[{"uri":"file:///home/synthetic/legacy.pdf","name":"legacy.pdf"}]},
        "attachments":[{"id":"original-file","filename":"shown.txt","mimeType":"text/plain","size":999,
            "source":"server","serverPath":"/never/read/server.txt","agentPath":"/never/read/original.txt","dataUrl":"data:text/plain;base64,aGk="}]}),
        &[],
        50,
    );
    let bindings = [QueuePreparedFile::new(
        QueuePayloadFileSource::Legacy(0),
        2,
        test_hash(b"hi"),
        "file:///tmp/owned/prepared.txt",
    )
    .expect("binding")];
    let send = capture(&client(), &[source(&doc, &index, &bindings)], 0).expect("legacy send");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("JSON");
    assert_eq!(
        body["files"][0],
        json!({"uri":"file:///tmp/owned/prepared.txt","name":"/never/read/original.txt"})
    );
    assert_eq!(
        body["files"][1],
        json!({"uri":"file:///home/synthetic/legacy.pdf","name":"legacy.pdf"})
    );
    assert_eq!(body["agents"], json!([{"name":"plan"}]));
    let wrong = [QueuePreparedFile::new(
        QueuePayloadFileSource::Legacy(0),
        2,
        test_hash(b"no"),
        "file:///tmp/owned/prepared.txt",
    )
    .expect("binding")];
    assert!(matches!(
        capture(&client(), &[source(&doc, &index, &wrong)], 0),
        Err(QueueFrozenSendError::FileMismatch)
    ));
    assert!(matches!(
        capture(&client(), &[source(&doc, &index, &[])], 0),
        Err(QueueFrozenSendError::FileMismatch)
    ));
}

#[test]
fn reference_limits_do_not_reject_original_prepared_legacy_model_facing_names() {
    for name in [
        format!("/model/{}.txt", "a".repeat(5000)),
        "/model/tab\tin-name.txt".to_owned(),
    ] {
        let (document, index) = sealed(
            json!({"content":"display only","text":"delivered first","sendConfig":config(),
            "attachments":[{"id":"original-file","filename":"shown.txt","mimeType":"text/plain","size":2,
                "agentPath":name,"dataUrl":"data:text/plain;base64,aGk="}]}),
            &[],
            100,
        );
        let files = [QueuePreparedFile::new(
            QueuePayloadFileSource::Legacy(0),
            2,
            test_hash(b"hi"),
            "file:///tmp/owned/prepared.txt",
        )
        .expect("actual prepared-byte binding")];
        let client = client();
        let send = capture(&client, &[source(&document, &index, &files)], 0)
            .expect("original legacy model-facing name remains valid");
        let body: Value = serde_json::from_slice(send.prompt_body()).expect("prompt JSON");
        assert_eq!(body["files"][0]["name"], name);
        let reopened = restore(&client, &send, send.bytes().to_vec(), send.sha256())
            .expect("retained frame stays recoverable");
        assert_eq!(reopened.bytes(), send.bytes());
    }
}

#[test]
fn manifest_model_identities_keep_each_shared_blob_occurrence_and_exact_frozen_frame() {
    let names = [
        "relative/../literal\\name?#%2F.txt".to_owned(),
        r"C:\original folder\source.txt".to_owned(),
        r"\\server\share\source.txt".to_owned(),
        " \tunchanged identity\t ".to_owned(),
        format!("/model/{}.txt", "a".repeat(5000)),
    ];
    let blob = [3; 32];
    let file = json!({"blobId":hex(&blob),"requestId":"file_original","attachmentId":"file",
        "filename":"safe.txt","mimeType":"text/plain","size":2,"sha256":hex(&test_hash(b"hi"))});
    let mut item = manifest();
    let mut files = Vec::new();
    for name in &names {
        let mut occurrence = file.clone();
        occurrence["agentPath"] = json!(name);
        files.push(occurrence);
    }
    files.push(file);
    item["attachmentManifest"] = Value::Array(files);
    let (document, index) = sealed(item, &[], 100_000);
    let original_bytes = document.bytes().to_vec();
    let proof = document.files_proof(test_hash).expect("one sealed root");
    assert_eq!(proof.files().len(), 1);
    let bindings = [QueuePreparedFile::new(
        QueuePayloadFileSource::Manifest(blob),
        2,
        test_hash(b"hi"),
        "file:///tmp/owned/safe.txt",
    )
    .expect("actual held-byte binding")];
    let client = client();
    let send =
        capture(&client, &[source(&document, &index, &bindings)], 0).expect("all model identities");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("frozen prompt");
    let expected = names
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("safe.txt"))
        .collect::<Vec<_>>();
    assert_eq!(
        body["files"]
            .as_array()
            .expect("file occurrences")
            .iter()
            .map(|file| file["name"].as_str().expect("model identity"))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        body["files"]
            .as_array()
            .expect("file occurrences")
            .iter()
            .all(|file| file["uri"] == "file:///tmp/owned/safe.txt"),
        "paths cannot replace the verified staging URI"
    );
    assert_eq!(
        send.metadata.prompt_bytes,
        send.prompt_body().len(),
        "actual escaped identities count in the request budget"
    );
    let restored =
        restore(&client, &send, send.bytes().to_vec(), send.sha256()).expect("exact frame restart");
    assert_eq!(restored.bytes(), send.bytes());
    assert_eq!(restored.prompt_body(), send.prompt_body());
    assert_eq!(document.bytes(), original_bytes);
    assert_eq!(
        document
            .files_proof(test_hash)
            .expect("unchanged proof")
            .capture_sha256(),
        proof.capture_sha256()
    );
}

#[test]
fn merge_keeps_order_and_original_selected_configuration_and_refuses_metadata_overwrite() {
    let mut first = manifest();
    first["promptParts"] =
        json!({"metadata":{"same":{"values":[1]},"first":true},"skills":[{"id":"skill_first"}]});
    let mut second = manifest();
    second["text"] = json!("delivered second");
    second["sendConfig"]["modelID"] = json!("composer-model");
    second["sendConfig"]["variant"] = json!("original-effort");
    second["promptParts"] = json!({"metadata":{"same":{"values":[1.0]},"second":true},"skills":[{"id":"skill_second"}]});
    let (first_doc, first_index) = sealed(first, &[], 100);
    let (second_doc, second_index) = sealed(second.clone(), &[], 100);
    let send = capture(
        &client(),
        &[
            source(&first_doc, &first_index, &[]),
            source(&second_doc, &second_index, &[]),
        ],
        1,
    )
    .expect("compatible merge");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("JSON");
    assert_eq!(body["text"], "delivered first\n\ndelivered second");
    assert_eq!(
        body["skills"],
        json!([{"id":"skill_first"},{"id":"skill_second"}])
    );
    assert_eq!(
        body["metadata"],
        json!({"same":{"values":[1]},"first":true,"second":true})
    );
    assert_eq!(send.configuration().model_id, "composer-model");
    assert_eq!(
        send.configuration().variant.as_deref(),
        Some("original-effort")
    );
    second["promptParts"]["metadata"]["same"] = json!({"different":true});
    let (bad_doc, bad_index) = sealed(second, &[], 100);
    assert!(matches!(
        capture(
            &client(),
            &[
                source(&first_doc, &first_index, &[]),
                source(&bad_doc, &bad_index, &[])
            ],
            1
        ),
        Err(QueueFrozenSendError::MetadataConflict)
    ));
}

// Literal ECMAScript JSON.stringify(JSON.parse(NUMERIC_INPUT)) spellings.
// The pinned @opencode/client 2.2.5 rpc-runtime-t2ajednw.js serializes bodies
// with JSON.stringify. The backend prompt-body.js uses that same operation for
// both body bytes and numeric length accounting. These expected bytes do not
// come from the Rust formatter under test. The >safe integer input tests the
// existing wire Number normalization, not permission to capture a Rust integer.
const NUMERIC_INPUT: &str = "[1.0,-0.0,1000,1e-7,1e-6,1e20,1e21,-1e-7,-1e21,9007199254740991,9007199254740992,9007199254740993,1000000000000000100]";
const NUMERIC_REQUEST: &str = "[1,0,1000,1e-7,0.000001,100000000000000000000,1e+21,-1e-7,-1e+21,9007199254740991,9007199254740992,9007199254740992,1000000000000000100]";

#[test]
fn numeric_metadata_matches_sdk_stringify_bytes_without_changing_typed_normalization() {
    let numbers: Value = serde_json::from_str(NUMERIC_INPUT).expect("numeric wire input");
    let mut item = manifest();
    item["context"] =
        json!([{"kind":"context","text":"legacy numbers","metadata":{"numbers":numbers}}]);
    item["promptParts"] = json!({"metadata":{"numbers":numbers},"context":[
        {"id":"msg_numbers_official","text":"official numbers","metadata":{"numbers":numbers}}]});
    let (doc, index) = sealed(item, &["msg_numbers_legacy", "msg_numbers_official"], 1000);
    let original_bytes = doc.bytes().to_vec();
    let QueuePayloadItem::Manifest(typed) = doc.item() else {
        panic!("manifest fixture");
    };
    assert!(
        typed.prompt_parts.metadata.as_ref().expect("metadata").0["numbers"]
            .as_array()
            .expect("numbers")
            .iter()
            .all(|number| number.as_number().expect("number").is_f64()),
        "capture keeps its established JavaScript f64 normalization"
    );

    let client = client();
    let send = capture(&client, &[source(&doc, &index, &[])], 0).expect("numeric send");
    let expected_prompt = format!(
        r#"{{"id":"{MESSAGE}","text":"delivered first","metadata":{{"numbers":{NUMERIC_REQUEST}}},"delivery":"steer"}}"#
    );
    assert_eq!(
        send.prompt_body(),
        expected_prompt.as_bytes(),
        "new prompt bytes match JSON.stringify, including notation thresholds and negative zero"
    );
    for (position, id, text) in [
        (0, "msg_numbers_legacy", "legacy numbers"),
        (1, "msg_numbers_official", "official numbers"),
    ] {
        let expected = format!(
            r#"{{"id":"{id}","text":"{text}","metadata":{{"numbers":{NUMERIC_REQUEST}}},"delivery":"steer","resume":false}}"#
        );
        let body = send.context_body(position).expect("context");
        assert_eq!(
            body,
            expected.as_bytes(),
            "synthetic metadata uses the same numeric writer"
        );
        assert_eq!(
            send.metadata.context[position].bytes,
            body.len(),
            "framed context length measures emitted bytes"
        );
    }
    assert_eq!(send.metadata.prompt_bytes, send.prompt_body().len());
    let parsed: Value = serde_json::from_slice(send.prompt_body()).expect("JSON body");
    assert_eq!(
        encoded_len(&parsed, send.prompt_body().len()).expect("inclusive size limit"),
        send.prompt_body().len()
    );
    assert!(
        matches!(
            encoded_len(&parsed, send.prompt_body().len() - 1),
            Err(QueueFrozenSendError::OutgoingTooLarge)
        ),
        "counting includes fixed-form 1e20 expansion and the positive exponent sign"
    );
    let reopened = restore(&client, &send, send.bytes().to_vec(), send.sha256())
        .expect("numeric reconstruction");
    assert_eq!(reopened.bytes(), send.bytes());
    assert_eq!(reopened.prompt_body(), expected_prompt.as_bytes());
    assert_eq!(reopened.context_ids(), send.context_ids());
    assert_eq!(
        doc.bytes(),
        original_bytes,
        "encoding leaves the original capture and digest input intact"
    );
}

#[test]
fn request_midpoint_digits_follow_the_independent_positive_and_negative_js_oracles() {
    // Exact 2^-25. The review's independent JSON.stringify oracle is also
    // independently pinned by zmij 1.0.23 tests/test.rs:94. Rust Display chooses
    // ...5313 at this midpoint; ECMAScript chooses nearest-even ...5312.
    let midpoint = f64::from_bits(0x3e60000000000000);
    for (value, expected) in [
        (midpoint, "2.9802322387695312e-8"),
        (-midpoint, "-2.9802322387695312e-8"),
    ] {
        let body = encode(&value, 64).expect("request number");
        assert_eq!(
            body,
            expected.as_bytes(),
            "nearest-even significant digits come from serde_json's supported formatter"
        );
        assert_eq!(
            encoded_len(&value, expected.len()).expect("exact count"),
            expected.len()
        );
        assert!(encoded_len(&value, expected.len() - 1).is_err());
    }
    let wire = json!({"content":"display only","text":"delivered first","sendConfig":config(),
        "context":[{"kind":"context","text":"context","metadata":{"positive":midpoint,"negative":-midpoint}}]});
    let document = QueuePayloadDocument::from_bytes(
        serde_json::to_vec(&json!({"directory":DIRECTORY,"item":wire})).expect("capture"),
        DIRECTORY,
        QueuePayloadUse::Composer,
        test_hash,
    )
    .expect("typed legacy source");
    let edited = QueuePayloadDocument::from_item(
        DIRECTORY,
        document.item(),
        QueuePayloadUse::Composer,
        test_hash,
    )
    .expect("compact edit");
    let compact = std::str::from_utf8(edited.bytes()).expect("UTF-8");
    assert!(
        compact.contains("2.9802322387695313e-8") && compact.contains("-2.9802322387695313e-8"),
        "existing compact capture spelling stays unchanged"
    );
}

#[test]
fn local_capture_restore_then_freeze_preserves_the_exact_javascript_number() {
    // Independent review counterexample. The Rust literal supplies the expected
    // nearest binary value without using the JSON parser being tested. The
    // decimal bytes are JSON.stringify's spelling for that exact JS Number.
    const ORIGINAL: f64 = 51.248178375505404;
    const DECIMAL: &str = "51.248178375505404";
    let metadata = Metadata(
        json!({"value":ORIGINAL})
            .as_object()
            .expect("metadata")
            .clone(),
    );
    let mut item = native_item();
    item.files.clear();
    item.prompt_parts.metadata = Some(metadata.clone());
    item.prompt_parts.context[0].metadata = Some(metadata);
    let local = QueueLocalCapture::new(
        DIRECTORY,
        item,
        vec![MessageId::new("msg_native_context")],
        test_hash,
    )
    .expect("local source");
    assert!(
        local
            .bytes()
            .windows(DECIMAL.len())
            .any(|bytes| bytes == DECIMAL.as_bytes()),
        "source contains the independent decimal oracle"
    );
    let restored = QueueLocalCapture::from_bytes(local.bytes().to_vec(), DIRECTORY, test_hash)
        .expect("source restore");
    for metadata in [
        restored
            .item()
            .prompt_parts
            .metadata
            .as_ref()
            .expect("prompt metadata"),
        restored.item().prompt_parts.context[0]
            .metadata
            .as_ref()
            .expect("context metadata"),
    ] {
        assert_eq!(
            metadata.0["value"].as_f64().expect("number").to_bits(),
            ORIGINAL.to_bits(),
            "correctly rounded parsing must not change 51.248178375505404 to 51.24817837550541"
        );
    }
    let client = client();
    let original_send = capture_mixed(
        &client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &local,
            files: &[],
        })],
        0,
    )
    .expect("original freeze");
    let restored_send = capture_mixed(
        &client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &restored,
            files: &[],
        })],
        0,
    )
    .expect("restored freeze");
    assert_eq!(
        restored_send.bytes(),
        original_send.bytes(),
        "spool restoration and freezing retain the original complete frame"
    );
    let frozen = restore(
        &client,
        &restored_send,
        restored_send.bytes().to_vec(),
        restored_send.sha256(),
    )
    .expect("frozen frame restore");
    let prompt: Value = serde_json::from_slice(frozen.prompt_body()).expect("prompt JSON");
    let context: Value =
        serde_json::from_slice(frozen.context_body(0).expect("context")).expect("context JSON");
    assert_eq!(
        prompt["metadata"]["value"]
            .as_f64()
            .expect("number")
            .to_bits(),
        ORIGINAL.to_bits()
    );
    assert_eq!(
        context["metadata"]["value"]
            .as_f64()
            .expect("number")
            .to_bits(),
        ORIGINAL.to_bits()
    );
    assert_eq!(frozen.prompt_body(), original_send.prompt_body());
    assert_eq!(frozen.context_body(0), original_send.context_body(0));
}

#[test]
fn numeric_request_formatter_does_not_bypass_captured_rust_integer_validation() {
    let mut item = manifest();
    item["promptParts"] = json!({"metadata":{"number":1}});
    let (doc, _) = sealed(item, &[], 100);
    for number in [
        9_007_199_254_740_991u64,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
    ] {
        let mut edited = doc.item().clone();
        let QueuePayloadItem::Manifest(item) = &mut edited else {
            panic!("manifest fixture");
        };
        item.prompt_parts
            .metadata
            .as_mut()
            .expect("metadata")
            .0
            .insert("number".to_owned(), json!(number));
        let capture = QueuePayloadDocument::from_item(
            DIRECTORY,
            &edited,
            QueuePayloadUse::Composer,
            test_hash,
        );
        assert_eq!(
            capture.is_ok(),
            number <= MAX_SAFE_INTEGER,
            "typed Rust integers are validated before any Number conversion"
        );
    }
}

#[test]
fn reconstruction_and_dispatch_keep_an_existing_frames_original_numeric_spelling() {
    let mut item = manifest();
    item["promptParts"] = json!({"metadata":{"number":1.0}});
    let (doc, index) = sealed(item, &[], 100);
    let client = client();
    let send = capture(&client, &[source(&doc, &index, &[])], 0).expect("numeric capture");
    let old_body = format!(
        r#"{{"id":"{MESSAGE}","text":"delivered first","metadata":{{"number":1.0}},"delivery":"steer"}}"#
    );
    let header_length =
        u32::from_be_bytes(send.bytes()[8..12].try_into().expect("header")) as usize;
    let mut frame: FrameMetadata =
        serde_json::from_slice(&send.bytes()[HEADER_BYTES..HEADER_BYTES + header_length])
            .expect("frame metadata");
    frame.sources = frame
        .sources
        .into_iter()
        .map(|source| match source {
            SourceMetadata::Current(source) => {
                let SourceProvenance::Queued { index_sha256 } = source.provenance else {
                    panic!("queued fixture");
                };
                SourceMetadata::Legacy(LegacySourceMetadata {
                    capture_sha256: source.capture_sha256,
                    index_sha256,
                    structured_bytes: source.structured_bytes,
                    configuration: source.configuration,
                })
            }
            source => source,
        })
        .collect();
    frame.prompt_bytes = old_body.len();
    let header = encode(&frame, QUEUE_MANIFEST_BYTES).expect("updated old frame lengths");
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(old_body.as_bytes());
    let digest = test_hash(&bytes);
    let reopened =
        restore(&client, &send, bytes.clone(), digest).expect("previous frame remains usable");
    assert_eq!(
        reopened.bytes(),
        bytes,
        "restoration never rewrites a durably captured request"
    );
    assert_eq!(reopened.sha256(), digest);
    assert_eq!(
        reopened.source_kinds().collect::<Vec<_>>(),
        [QueueFrozenSourceKind::LegacyFrame],
        "old frames do not acquire new source authority"
    );
    let request = reopened
        .request("prompt", reopened.prompt_body())
        .build()
        .expect("captured dispatch request");
    assert_eq!(
        request.body().and_then(reqwest::Body::as_bytes),
        Some(old_body.as_bytes()),
        "explicit retry sends the original body bytes"
    );
}

#[test]
fn old_large_exponent_frame_restores_by_its_original_bytes_while_new_capture_preflight_still_refuses()
 {
    const COUNT: usize = 6_500_000;
    let mut item = manifest();
    item["promptParts"] =
        json!({"metadata":{"values":Value::Array(vec![Value::from(1e20_f64); COUNT])}});
    // Backend logical structured size uses compact 1e20, the metadata object,
    // its array punctuation, empty agents/skills and the delivered text.
    let structured = (COUNT * 5 + 1 + b"{\"values\":}".len() + 4 + "delivered first".len()) as u64;
    let (document, index) = sealed(item, &[], structured);
    assert!(
        document.bytes().len() <= QUEUE_PAYLOAD_BYTES,
        "original capture remains within 50 MiB"
    );
    let client = client();
    assert!(
        matches!(
            capture(&client, &[source(&document, &index, &[])], 0),
            Err(QueueFrozenSendError::OutgoingTooLarge)
        ),
        "new request preflight counts every emitted 21-digit fixed-form number before allocating a frame"
    );
    assert!(
        COUNT * 22 > QUEUE_ADMISSION_BYTES as usize,
        "new fixed-form array alone exceeds 128 MiB"
    );

    // Real old-format JSON bytes, not a mocked size counter. Each original
    // 1e+20 occurrence occupies 5 bytes and must remain unchanged on retry.
    let prefix = format!(r#"{{"id":"{MESSAGE}","text":"delivered first","metadata":{{"values":["#);
    let suffix = b"]},\"delivery\":\"steer\"}";
    let mut body = Vec::with_capacity(prefix.len() + COUNT * 6 - 1 + suffix.len());
    body.extend_from_slice(prefix.as_bytes());
    for index in 0..COUNT {
        if index != 0 {
            body.push(b',');
        }
        body.extend_from_slice(b"1e+20");
    }
    body.extend_from_slice(suffix);
    assert!(
        body.len() < QUEUE_PAYLOAD_BYTES,
        "actual old request is below 50 MiB"
    );
    let metadata = FrameMetadata {
        endpoint: client.endpoint().as_str().to_owned(),
        session: SESSION.to_owned(),
        directory: DIRECTORY.to_owned(),
        message_id: MESSAGE.to_owned(),
        selected_input: 0,
        sources: vec![SourceMetadata::Legacy(LegacySourceMetadata {
            capture_sha256: document.sha256(),
            index_sha256: index.index_hash(),
            structured_bytes: structured,
            configuration: Configuration::capture(document.item().send_config()),
        })],
        delivery: Some(Delivery::Steer),
        context: vec![],
        prompt_bytes: body.len(),
        file_reference_ordinals: Vec::new(),
    };
    let header = encode(&metadata, QUEUE_MANIFEST_BYTES).expect("bounded old metadata");
    let mut bytes = Vec::with_capacity(HEADER_BYTES + header.len() + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&body);
    assert!(
        bytes.len() <= QUEUE_ADMISSION_BYTES as usize,
        "actual old frame stays within 128 MiB"
    );
    let digest = test_hash(&bytes);
    let configuration = document.item().send_config().clone();
    // Keep only the original frame and request buffers during restoration's
    // bounded parse. The complete source and index have served their purpose.
    drop(document);
    drop(index);
    let frozen = QueueFrozenSend::from_bytes(
        &client,
        QueueFrozenSendOwner {
            session: &SessionId::new(SESSION),
            directory: DIRECTORY,
            message_id: &MessageId::new(MESSAGE),
            configuration: &configuration,
        },
        bytes,
        digest,
        test_hash,
    )
    .expect("old metadata structure is valid regardless of today's number notation");
    assert_eq!(frozen.sha256(), digest);
    assert_eq!(frozen.prompt_body(), body);
    assert_eq!(
        frozen.source_kinds().collect::<Vec<_>>(),
        [QueueFrozenSourceKind::LegacyFrame]
    );
    let request = frozen
        .request("prompt", frozen.prompt_body())
        .build()
        .expect("original retained request");
    assert_eq!(
        request.body().and_then(reqwest::Body::as_bytes),
        Some(body.as_slice()),
        "explicit retry uses the exact original exponent spellings"
    );
}

fn native_item() -> QueueLocalItem {
    QueueLocalItem {
        content: "native display only".to_owned(),
        text: "native delivered text".to_owned(),
        files: vec![QueueLocalFile {
            name: "C:\\original folder\\native.txt".to_owned(),
            mime_type: "text/plain".to_owned(),
            byte_len: 2,
            sha256: test_hash(b"hi"),
            description: Some("native description".to_owned()),
            mention: Some(QueueMention {
                start: 4_294_967_297,
                end: 4_294_967_300,
                text: "@native".to_owned(),
            }),
        }],
        send_config: QueuedSendConfig {
            provider_id: "provider".to_owned(),
            model_id: "native-model".to_owned(),
            agent: Some("build".to_owned()),
            variant: Some("native-effort".to_owned()),
        },
        prompt_parts: QueuePromptParts {
            agents: vec![QueueAgent {
                name: "plan".to_owned(),
                mention: None,
            }],
            file_references: vec![],
            skills: vec![QueueSkill {
                id: "native-skill".to_owned(),
                mention: Some(QueueMention {
                    start: 10,
                    end: 16,
                    text: "$skill".to_owned(),
                }),
            }],
            metadata: Some(Metadata(
                json!({"native":true}).as_object().expect("object").clone(),
            )),
            context: vec![QueuePromptContext {
                id: Some(MessageId::new("msg_native_context")),
                text: "native context".to_owned(),
                description: Some("native context description".to_owned()),
                metadata: None,
            }],
        },
    }
}

fn native_capture() -> QueueLocalCapture {
    QueueLocalCapture::new(
        DIRECTORY,
        native_item(),
        vec![MessageId::new("msg_native_context")],
        test_hash,
    )
    .expect("native capture without any queue descriptors")
}

fn native_files() -> Vec<QueuePreparedFile> {
    vec![
        QueuePreparedFile::new_local(0, 2, test_hash(b"hi"), "file:///tmp/owned/native.txt")
            .expect("local ordinal binding"),
    ]
}

fn capture_mixed(
    client: &ServerClient,
    sources: &[QueueFrozenSource<'_>],
    selected: usize,
) -> Result<QueueFrozenSend, QueueFrozenSendError> {
    QueueFrozenSend::capture_local(
        client,
        QueueLocalSendInput {
            session: &SessionId::new(SESSION),
            directory: DIRECTORY,
            message_id: &MessageId::new(MESSAGE),
            sources,
            selected_input: selected,
            delivery: Some(Delivery::Steer),
        },
        test_hash,
    )
}

fn native_reference() -> QueueFileReference {
    QueueFileReference {
        uri: "file:///home/synthetic/native.pdf".to_owned(),
        name: "native.pdf".to_owned(),
        description: Some("keep this reference".to_owned()),
        mention: Some(QueueMention {
            start: 5,
            end: 12,
            text: "@native".to_owned(),
        }),
    }
}

#[test]
fn mixed_queued_and_local_references_keep_occurrences_after_each_sources_prepared_files_and_restore_exactly()
 {
    let mut queued = manifest();
    queued["promptParts"] =
        json!({"fileReferences":[{"uri":"file:///home/synthetic/queued.pdf","name":"queued.pdf"}]});
    let (document, index) = sealed(queued, &[], 80);
    let mut item = native_item();
    item.prompt_parts.file_references = vec![native_reference(), native_reference()];
    let native = QueueLocalCapture::new(
        DIRECTORY,
        item,
        vec![MessageId::new("msg_native_context")],
        test_hash,
    )
    .expect("local references");
    let files = native_files();
    let client = client();
    let send = capture_mixed(
        &client,
        &[
            QueueFrozenSource::Queued(source(&document, &index, &[])),
            QueueFrozenSource::Local(QueueLocalFrozenItem {
                capture: &native,
                files: &files,
            }),
        ],
        1,
    )
    .expect("mixed references");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("prompt JSON");
    assert_eq!(body["files"].as_array().expect("files").len(), 4);
    assert_eq!(body["files"][0]["name"], "queued.pdf");
    assert_eq!(body["files"][1]["uri"], "file:///tmp/owned/native.txt");
    assert_eq!(
        body["files"][2],
        serde_json::to_value(native_reference()).expect("reference JSON")
    );
    assert_eq!(
        body["files"][2], body["files"][3],
        "repeated mentions retain separate occurrences"
    );
    assert_eq!(
        send.metadata.file_reference_ordinals,
        [0, 2, 3],
        "prepared and reference occurrences have separate frame validation"
    );
    assert_eq!(
        send.metadata.prompt_bytes,
        send.prompt_body().len(),
        "actual body count includes references"
    );
    let mut without_refs = native.item().clone();
    without_refs.prompt_parts.file_references.clear();
    assert_eq!(
        local_structured_bytes(native.item()).expect("with refs")
            - local_structured_bytes(&without_refs).expect("without refs"),
        compact_queue_json_len(&native.item().prompt_parts.file_references)
            .expect("reference count") as u64
    );
    let reopened =
        restore(&client, &send, send.bytes().to_vec(), send.sha256()).expect("mixed restart");
    assert_eq!(reopened.prompt_body(), send.prompt_body());
    assert!(
        document
            .files_proof(test_hash)
            .expect("sealed proof")
            .files()
            .is_empty(),
        "queued references add no byte ownership"
    );
}

#[test]
fn native_local_large_display_edit_keeps_the_50_mib_capture_bound_and_queue_content_limit() {
    let mut item = QueueLocalItem {
        content: "x".repeat(200001),
        text: "edited".to_owned(),
        files: vec![],
        send_config: native_item().send_config,
        prompt_parts: QueuePromptParts::default(),
    };
    let capture = QueueLocalCapture::new(DIRECTORY, item.clone(), vec![], test_hash)
        .expect("200001 ASCII local display edit");
    assert_eq!(
        QueueLocalCapture::from_bytes(capture.bytes().to_vec(), DIRECTORY, test_hash)
            .expect("large local restart")
            .item()
            .content,
        item.content
    );
    let empty = QueueLocalItem {
        content: String::new(),
        ..item.clone()
    };
    let overhead = QueueLocalCapture::new(DIRECTORY, empty, vec![], test_hash)
        .expect("overhead")
        .bytes()
        .len();
    item.content = "x".repeat(QUEUE_PAYLOAD_BYTES - overhead);
    let capture = QueueLocalCapture::new(DIRECTORY, item.clone(), vec![], test_hash)
        .expect("whole-capture equality");
    assert_eq!(capture.bytes().len(), QUEUE_PAYLOAD_BYTES);
    drop(capture);
    item.content.push('x');
    assert!(matches!(
        QueueLocalCapture::new(DIRECTORY, item, vec![], test_hash),
        Err(crate::queue_manifest::QueueManifestError::PayloadTooLarge)
    ));
    let mut queued = manifest();
    queued["content"] = json!("x".repeat(200001));
    assert!(
        QueuePayloadDocument::from_bytes(
            serde_json::to_vec(&json!({"directory":DIRECTORY,"item":queued})).expect("JSON"),
            DIRECTORY,
            QueuePayloadUse::Composer,
            test_hash
        )
        .is_err(),
        "queue manifest display content keeps its 200000 UTF-16 limit"
    );
}

#[tokio::test]
async fn reference_only_empty_text_send_restarts_and_dispatches_no_file_reads_uploads_or_queue_requests()
 {
    let server = Server::start(accept).await;
    let item = QueueLocalItem {
        content: String::new(),
        text: String::new(),
        files: vec![],
        send_config: native_item().send_config,
        prompt_parts: QueuePromptParts {
            file_references: vec![native_reference()],
            ..QueuePromptParts::default()
        },
    };
    let local = QueueLocalCapture::new(DIRECTORY, item, vec![], test_hash)
        .expect("reference-only local capture");
    let send = capture_mixed(
        &server.client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &local,
            files: &[],
        })],
        0,
    )
    .expect("reference-only empty text prompt");
    let restored = restore(&server.client, &send, send.bytes().to_vec(), send.sha256())
        .expect("reference-only restart");
    restored
        .dispatch(&[])
        .await
        .expect("reference-only admission");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "only model, agent and prompt are dispatched"
    );
    assert_eq!(requests[2].body, send.prompt_body());
    let prompt: Value = serde_json::from_slice(&requests[2].body).expect("prompt JSON");
    assert_eq!(prompt["text"], "");
    assert_eq!(prompt["files"], json!([native_reference()]));
}

#[test]
fn restored_reference_frames_reject_unsafe_uris_and_reference_counts_share_the_existing_limit() {
    let item = QueueLocalItem {
        content: String::new(),
        text: String::new(),
        files: vec![],
        send_config: native_item().send_config,
        prompt_parts: QueuePromptParts {
            file_references: vec![native_reference()],
            ..QueuePromptParts::default()
        },
    };
    let local = QueueLocalCapture::new(DIRECTORY, item.clone(), vec![], test_hash)
        .expect("reference source");
    let client = client();
    let send = capture_mixed(
        &client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &local,
            files: &[],
        })],
        0,
    )
    .expect("reference-only send");
    let header_len = u32::from_be_bytes(send.bytes()[8..12].try_into().expect("header")) as usize;
    for uri in [
        "https://host/a",
        "file://host/a",
        "file:///a?query",
        "file:///a#fragment",
        "file:///a\\b",
        "file:///a%xx",
    ] {
        let mut body: Value = serde_json::from_slice(send.prompt_body()).expect("prompt");
        body["files"][0]["uri"] = json!(uri);
        let body = serde_json::to_vec(&body).expect("forged prompt");
        let mut header: FrameMetadata =
            serde_json::from_slice(&send.bytes()[HEADER_BYTES..HEADER_BYTES + header_len])
                .expect("metadata");
        header.prompt_bytes = body.len();
        let header = encode(&header, QUEUE_MANIFEST_BYTES).expect("metadata");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&body);
        let digest = test_hash(&bytes);
        assert!(
            restore(&client, &send, bytes, digest).is_err(),
            "a correct digest never legitimizes an unsafe reference"
        );
    }
    for ordinals in [vec![0, 0], vec![1]] {
        let mut header: FrameMetadata =
            serde_json::from_slice(&send.bytes()[HEADER_BYTES..HEADER_BYTES + header_len])
                .expect("metadata");
        header.file_reference_ordinals = ordinals;
        let header = encode(&header, QUEUE_MANIFEST_BYTES).expect("bounded malformed metadata");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(send.prompt_body());
        let digest = test_hash(&bytes);
        assert!(
            restore(&client, &send, bytes, digest).is_err(),
            "reference ordinals must be unique, ordered and in bounds"
        );
    }
    for name in ["a".repeat(4097), "tab\tin-name.pdf".to_owned()] {
        let mut body: Value = serde_json::from_slice(send.prompt_body()).expect("prompt");
        body["files"][0]["name"] = json!(name);
        let body = serde_json::to_vec(&body).expect("modified reference body");
        let mut header: FrameMetadata =
            serde_json::from_slice(&send.bytes()[HEADER_BYTES..HEADER_BYTES + header_len])
                .expect("metadata");
        header.prompt_bytes = body.len();
        let header = encode(&header, QUEUE_MANIFEST_BYTES).expect("bounded metadata");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&body);
        let digest = test_hash(&bytes);
        assert!(
            restore(&client, &send, bytes, digest).is_err(),
            "identified references retain strict name limits"
        );
    }
    let mut many = item;
    many.prompt_parts.file_references = vec![native_reference(); QUEUE_MANIFEST_ENTRIES / 2 + 1];
    let many = QueueLocalCapture::new(DIRECTORY, many, vec![], test_hash).expect("bounded source");
    assert!(
        capture_mixed(
            &client,
            &[
                QueueFrozenSource::Local(QueueLocalFrozenItem {
                    capture: &many,
                    files: &[]
                }),
                QueueFrozenSource::Local(QueueLocalFrozenItem {
                    capture: &many,
                    files: &[]
                })
            ],
            0
        )
        .is_err(),
        "combined references share the 4096-entry limit"
    );
}

#[test]
fn local_context_rebinding_preserves_rich_fields_and_recaptures_exact_order() {
    let mut item = native_item();
    item.prompt_parts.file_references = vec![native_reference(), native_reference()];
    item.prompt_parts.context[0].metadata = Some(Metadata(
        json!({"nested":{"number":1.5,"values":[true,null,"retained"]}})
            .as_object()
            .expect("context metadata")
            .clone(),
    ));
    item.prompt_parts.context.push(QueuePromptContext {
        id: Some(MessageId::new("msg_original_second")),
        text: "second context".to_owned(),
        description: Some("second description".to_owned()),
        metadata: Some(Metadata(
            json!({"second":true})
                .as_object()
                .expect("metadata")
                .clone(),
        )),
    });
    let original_ids = vec![
        MessageId::new("msg_native_context"),
        MessageId::new("msg_original_second"),
    ];
    let capture = QueueLocalCapture::new(DIRECTORY, item, original_ids, test_hash)
        .expect("rich local capture");
    let original_bytes = capture.bytes().to_vec();
    let original_digest = capture.sha256();
    let mut expected = capture.item().clone();
    let ids = vec![MessageId::new("msg_reuse_z"), MessageId::new("msg_reuse_a")];
    for (context, id) in expected.prompt_parts.context.iter_mut().zip(&ids) {
        context.id = Some(id.clone());
    }
    let rebound = capture
        .rebind_context_ids(ids.clone(), test_hash)
        .expect("valid rebinding");
    assert_eq!(rebound.directory(), DIRECTORY);
    assert_eq!(rebound.context_ids(), ids);
    assert_eq!(rebound.item().content, expected.content);
    assert_eq!(rebound.item().text, expected.text);
    assert_eq!(rebound.item().files, expected.files);
    assert_eq!(rebound.item().send_config, expected.send_config);
    assert_eq!(rebound.item().prompt_parts, expected.prompt_parts);
    assert_ne!(rebound.bytes(), original_bytes);
    assert_ne!(rebound.sha256(), original_digest);
    assert_eq!(rebound.sha256(), test_hash(rebound.bytes()));
    let restored = QueueLocalCapture::from_bytes(rebound.bytes().to_vec(), DIRECTORY, test_hash)
        .expect("rebound source restoration");
    assert_eq!(restored.bytes(), rebound.bytes());
    assert_eq!(restored.sha256(), rebound.sha256());
    assert_eq!(restored.context_ids(), ids);
    assert_eq!(
        serde_json::to_value(restored.item()).expect("restored fields"),
        serde_json::to_value(rebound.item()).expect("rebound fields")
    );
}

#[test]
fn identical_local_context_bindings_preserve_original_number_spellings_including_empty_binding() {
    for has_context in [false, true] {
        let mut item = native_item();
        let ids = if has_context {
            vec![MessageId::new("msg_native_context")]
        } else {
            item.prompt_parts.context.clear();
            vec![]
        };
        let capture =
            QueueLocalCapture::new(DIRECTORY, item, ids.clone(), test_hash).expect("local capture");
        let original = std::str::from_utf8(capture.bytes())
            .expect("source JSON")
            .replace(r#""native":true"#, r#""native":1.000"#)
            .into_bytes();
        assert!(
            std::str::from_utf8(&original)
                .expect("original JSON")
                .contains("1.000")
        );
        let restored = QueueLocalCapture::from_bytes(original.clone(), DIRECTORY, test_hash)
            .expect("earlier numeric spelling");
        let digest = restored.sha256();
        let rebound = restored
            .rebind_context_ids(ids.clone(), |_| {
                panic!("identical IDs must retain the original digest without recapturing")
            })
            .expect("identical binding");
        assert_eq!(rebound.bytes(), original);
        assert_eq!(rebound.sha256(), digest);
        assert_eq!(rebound.context_ids(), ids);
    }
}

#[test]
fn local_context_rebinding_rejects_wrong_counts_duplicate_and_invalid_ids() {
    let mut item = native_item();
    item.prompt_parts.context.push(QueuePromptContext {
        id: Some(MessageId::new("msg_original_second")),
        text: "second context".to_owned(),
        description: None,
        metadata: None,
    });
    let original_ids = vec![
        MessageId::new("msg_native_context"),
        MessageId::new("msg_original_second"),
    ];
    for values in [
        vec![],
        vec!["msg_one"],
        vec!["msg_one", "msg_two", "msg_three"],
        vec!["msg_duplicate", "msg_duplicate"],
        vec!["bad_context", "msg_valid"],
        vec!["msg_valid", "msg_"],
        vec!["msg_has space", "msg_valid"],
    ] {
        let capture =
            QueueLocalCapture::new(DIRECTORY, item.clone(), original_ids.clone(), test_hash)
                .expect("original binding");
        let ids = values.into_iter().map(MessageId::new).collect();
        assert!(matches!(
            capture.rebind_context_ids(ids, test_hash),
            Err(crate::queue_manifest::QueueManifestError::InvalidInput)
        ));
    }
    assert!(
        QueueLocalCapture::new(
            DIRECTORY,
            item,
            vec![MessageId::new("msg_new_one"), MessageId::new("msg_new_two")],
            test_hash
        )
        .is_err(),
        "ordinary construction still refuses replacement of assigned IDs"
    );
    let mut empty = native_item();
    empty.prompt_parts.context.clear();
    let empty = QueueLocalCapture::new(DIRECTORY, empty, vec![], test_hash).expect("empty binding");
    assert!(matches!(
        empty.rebind_context_ids(vec![MessageId::new("msg_extra")], test_hash),
        Err(crate::queue_manifest::QueueManifestError::InvalidInput)
    ));
}

#[test]
fn materialized_local_context_rebinding_preserves_original_queue_capture_index_and_file_proof() {
    let mut item = manifest();
    item["attachmentManifest"] = json!([{"blobId":hex(&[3; 32]),"requestId":"file_original",
        "attachmentId":"file","filename":"notes.txt","mimeType":"text/plain","size":2,
        "sha256":hex(&test_hash(b"hi")),"description":"original file description"}]);
    item["context"] = json!([{"kind":"context","instructions":"read first",
        "text":"legacy context","metadata":{"original":true}}]);
    item["promptParts"] = json!({"context":[{"id":"msg_official_original","text":"official context",
        "description":"official description","metadata":{"nested":[1,true]}}]});
    let ids = vec![
        MessageId::new("msg_instruction_original"),
        MessageId::new("msg_legacy_original"),
        MessageId::new("msg_official_original"),
    ];
    let (document, index) = sealed(
        item,
        &[
            "msg_instruction_original",
            "msg_legacy_original",
            "msg_official_original",
        ],
        200,
    );
    let original_bytes = document.bytes().to_vec();
    let original_digest = document.sha256();
    let original_item = document.item().clone();
    let original_index = index.body().expect("original index");
    let proof = document
        .files_proof(test_hash)
        .expect("original file proof");
    let working = index
        .materialize(original_bytes.clone(), QueuePayloadUse::Composer, test_hash)
        .expect("genuine working materialization");
    let local = QueueLocalCapture::from_document(working, ids.clone(), test_hash)
        .expect("explicit working local conversion");
    assert_eq!(
        local.context_ids(),
        ids,
        "first consumer keeps the original IDs"
    );
    let files = local.item().files.clone();
    let rebound_ids = vec![
        MessageId::new("msg_reuse_instruction"),
        MessageId::new("msg_reuse_legacy"),
        MessageId::new("msg_reuse_official"),
    ];
    let rebound = local
        .rebind_context_ids(rebound_ids.clone(), test_hash)
        .expect("later consumer");
    assert_eq!(rebound.context_ids(), rebound_ids);
    assert_eq!(rebound.item().files, files);
    assert_eq!(
        rebound
            .item()
            .prompt_parts
            .context
            .iter()
            .map(|context| context.text.as_str())
            .collect::<Vec<_>>(),
        ["read first", "legacy context", "official context"]
    );
    assert_eq!(document.bytes(), original_bytes);
    assert_eq!(document.sha256(), original_digest);
    assert_eq!(document.item(), &original_item);
    assert_eq!(index.body().expect("unchanged index"), original_index);
    assert_eq!(index.index().context_ids, ids);
    assert_eq!(proof.capture_sha256(), &original_digest);
    assert_eq!(proof.index_sha256(), &index.index_hash());
    assert_eq!(proof.files()[0].sha256(), &test_hash(b"hi"));
    assert!(crate::queue_manifest::QueuePayloadPointer::from_body(rebound.bytes()).is_err());
    assert!(
        QueuePayloadDocument::from_bytes(
            rebound.bytes().to_vec(),
            DIRECTORY,
            QueuePayloadUse::Composer,
            test_hash
        )
        .is_err(),
        "local rebinding grants no queue proof"
    );
}

#[test]
fn empty_queue_local_capture_preserves_rich_fields_and_has_no_queue_index_or_upload_identity() {
    let native = native_capture();
    let reopened_native =
        QueueLocalCapture::from_bytes(native.bytes().to_vec(), DIRECTORY, test_hash)
            .expect("native source spool restore");
    assert_eq!(reopened_native.bytes(), native.bytes());
    assert_eq!(reopened_native.context_ids(), native.context_ids());
    assert_eq!(reopened_native.item().content, "native display only");
    let files = native_files();
    let send = capture_mixed(
        &client(),
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &reopened_native,
            files: &files,
        })],
        0,
    )
    .expect("ordinary empty-queue rich send");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("JSON");
    assert_eq!(body["text"], "native delivered text");
    assert_eq!(
        body["files"][0],
        json!({"uri":"file:///tmp/owned/native.txt","name":"C:\\original folder\\native.txt", "description":"native description",
        "mention":{"start":4294967297u64,"end":4294967300u64,"text":"@native"}})
    );
    assert_eq!(body["skills"][0]["mention"]["text"], "$skill");
    assert_eq!(
        send.context_ids(),
        vec![MessageId::new("msg_native_context")]
    );
    assert_eq!(
        send.source_kinds().collect::<Vec<_>>(),
        [QueueFrozenSourceKind::Local]
    );
    assert!(matches!(
        send.metadata.sources[0],
        SourceMetadata::Current(CurrentSourceMetadata {
            provenance: SourceProvenance::Local {},
            ..
        })
    ));
    let local_json = std::str::from_utf8(native.bytes()).expect("local source JSON");
    for absent in [
        "blobId",
        "requestId",
        "attachmentId",
        "payloadIndex",
        "indexSha256",
    ] {
        assert!(
            !local_json.contains(absent),
            "local source has no fabricated {absent}"
        );
    }
    assert!(
        crate::queue_manifest::QueuePayloadPointer::from_body(native.bytes()).is_err(),
        "local capture cannot become queue acknowledgement evidence"
    );
    assert!(
        QueuePayloadDocument::from_bytes(
            native.bytes().to_vec(),
            DIRECTORY,
            QueuePayloadUse::Composer,
            test_hash
        )
        .is_err(),
        "local spool has a distinct source format"
    );
}

#[test]
fn native_and_sealed_companion_share_the_codec_but_keep_order_and_separate_authority() {
    let mut item = manifest();
    item["promptParts"] = json!({"skills":[{"id":"companion-skill","mention":{"start":0,"end":5,"text":"$kept"}}],
        "metadata":{"companion":true},"context":[{"id":"msg_companion_original","text":"companion context","description":"kept description"}]});
    let (document, index) = sealed(item, &["msg_companion_original"], 100);
    let native = native_capture();
    let files = native_files();
    let sources = [
        QueueFrozenSource::Queued(source(&document, &index, &[])),
        QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &native,
            files: &files,
        }),
    ];
    let client = client();
    let send = capture_mixed(&client, &sources, 1).expect("mixed capture");
    let body: Value = serde_json::from_slice(send.prompt_body()).expect("JSON");
    assert_eq!(body["text"], "delivered first\n\nnative delivered text");
    assert_eq!(body["skills"][0]["id"], "companion-skill");
    assert_eq!(body["skills"][1]["id"], "native-skill");
    assert_eq!(
        send.configuration().variant.as_deref(),
        Some("native-effort")
    );
    assert_eq!(
        send.context_ids(),
        vec![
            MessageId::new("msg_companion_original"),
            MessageId::new("msg_native_context")
        ]
    );
    assert_eq!(
        send.source_kinds().collect::<Vec<_>>(),
        [QueueFrozenSourceKind::Queued, QueueFrozenSourceKind::Local]
    );
    let restored =
        restore(&client, &send, send.bytes().to_vec(), send.sha256()).expect("mixed frame restart");
    assert_eq!(restored.bytes(), send.bytes());
    assert_eq!(
        restored.source_kinds().collect::<Vec<_>>(),
        send.source_kinds().collect::<Vec<_>>()
    );
    let unsealed = QueuePayloadDocument::from_bytes(
        document.bytes().to_vec(),
        DIRECTORY,
        QueuePayloadUse::Composer,
        test_hash,
    )
    .expect("unsealed parse");
    assert!(
        capture_mixed(
            &client,
            &[
                QueueFrozenSource::Queued(source(&unsealed, &index, &[])),
                QueueFrozenSource::Local(QueueLocalFrozenItem {
                    capture: &native,
                    files: &files
                })
            ],
            1
        )
        .is_err(),
        "mixing never bypasses the original sealed queue precondition"
    );
    let edited = QueueLocalCapture::from_document(
        document,
        vec![MessageId::new("msg_companion_original")],
        test_hash,
    )
    .expect("explicit local edit conversion");
    let local = capture_mixed(
        &client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &edited,
            files: &[],
        })],
        0,
    )
    .expect("explicit local edit send");
    assert_eq!(
        local.source_kinds().collect::<Vec<_>>(),
        [QueueFrozenSourceKind::Local],
        "local conversion does not retain a queue index or acknowledgement capability"
    );
}

#[test]
fn local_capture_rejects_invalid_ids_file_bindings_unsafe_integers_and_metadata_conflicts() {
    for ids in [
        vec![],
        vec![MessageId::new("bad_context")],
        vec![MessageId::new("msg_other")],
    ] {
        assert!(QueueLocalCapture::new(DIRECTORY, native_item(), ids, test_hash).is_err());
    }
    let mut duplicated = native_item();
    duplicated
        .prompt_parts
        .context
        .push(duplicated.prompt_parts.context[0].clone());
    assert!(
        QueueLocalCapture::new(
            DIRECTORY,
            duplicated,
            vec![MessageId::new("msg_native_context"); 2],
            test_hash
        )
        .is_err()
    );
    let mut unsafe_item = native_item();
    unsafe_item
        .prompt_parts
        .metadata
        .as_mut()
        .expect("metadata")
        .0
        .insert("unsafe".to_owned(), json!(9_007_199_254_740_993u64));
    assert!(
        QueueLocalCapture::new(
            DIRECTORY,
            unsafe_item,
            vec![MessageId::new("msg_native_context")],
            test_hash
        )
        .is_err()
    );
    let mut huge_file = native_item();
    huge_file.files[0].byte_len = QUEUE_BLOB_BYTES as u64 + 1;
    assert!(
        QueueLocalCapture::new(
            DIRECTORY,
            huge_file,
            vec![MessageId::new("msg_native_context")],
            test_hash
        )
        .is_err()
    );
    let native = native_capture();
    let wrong_queued = QueuePreparedFile::new(
        QueuePayloadFileSource::Legacy(0),
        2,
        test_hash(b"hi"),
        "file:///tmp/owned/native.txt",
    )
    .expect("queued binding");
    for files in [
        vec![],
        vec![wrong_queued],
        vec![
            QueuePreparedFile::new_local(1, 2, test_hash(b"hi"), "file:///tmp/owned/native.txt")
                .expect("wrong ordinal"),
        ],
        vec![
            QueuePreparedFile::new_local(0, 2, test_hash(b"no"), "file:///tmp/owned/native.txt")
                .expect("wrong bytes"),
        ],
    ] {
        assert!(matches!(
            capture_mixed(
                &client(),
                &[QueueFrozenSource::Local(QueueLocalFrozenItem {
                    capture: &native,
                    files: &files
                })],
                0
            ),
            Err(QueueFrozenSendError::FileMismatch)
        ));
    }
    let mut bad_context = native_item();
    bad_context.prompt_parts.context[0].id = Some(MessageId::new(MESSAGE));
    let main_id_context = QueueLocalCapture::new(
        DIRECTORY,
        bad_context,
        vec![MessageId::new(MESSAGE)],
        test_hash,
    )
    .expect("valid local IDs before prompt binding");
    assert!(
        capture_mixed(
            &client(),
            &[QueueFrozenSource::Local(QueueLocalFrozenItem {
                capture: &main_id_context,
                files: &native_files()
            })],
            0
        )
        .is_err()
    );
    let mut conflict = native_item();
    conflict.files.clear();
    conflict.prompt_parts.context.clear();
    conflict
        .prompt_parts
        .metadata
        .as_mut()
        .expect("metadata")
        .0
        .insert("native".to_owned(), json!(false));
    let conflict = QueueLocalCapture::new(DIRECTORY, conflict, vec![], test_hash)
        .expect("local conflict source");
    assert!(matches!(
        capture_mixed(
            &client(),
            &[
                QueueFrozenSource::Local(QueueLocalFrozenItem {
                    capture: &native,
                    files: &native_files()
                }),
                QueueFrozenSource::Local(QueueLocalFrozenItem {
                    capture: &conflict,
                    files: &[]
                })
            ],
            0
        ),
        Err(QueueFrozenSendError::MetadataConflict)
    ));
}

#[test]
fn local_capture_size_is_inclusive_and_local_provenance_cannot_contain_an_index_hash() {
    let empty = || QueueLocalItem {
        content: String::new(),
        text: String::new(),
        files: vec![],
        send_config: native_item().send_config,
        prompt_parts: QueuePromptParts::default(),
    };
    let overhead = QueueLocalCapture::new(DIRECTORY, empty(), vec![], test_hash)
        .expect("empty source capture")
        .bytes()
        .len();
    let mut item = empty();
    item.text = "x".repeat(QUEUE_PAYLOAD_BYTES - overhead);
    let capture =
        QueueLocalCapture::new(DIRECTORY, item, vec![], test_hash).expect("50 MiB equality");
    assert_eq!(capture.bytes().len(), QUEUE_PAYLOAD_BYTES);
    assert!(
        capture
            .bytes()
            .chunks(QUEUE_BLOB_BYTES)
            .all(|chunk| !chunk.is_empty() && chunk.len() <= QUEUE_BLOB_BYTES)
    );
    drop(capture);
    let mut item = empty();
    item.text = "x".repeat(QUEUE_PAYLOAD_BYTES - overhead + 1);
    assert!(matches!(
        QueueLocalCapture::new(DIRECTORY, item, vec![], test_hash),
        Err(crate::queue_manifest::QueueManifestError::PayloadTooLarge)
    ));

    let native = native_capture();
    let files = native_files();
    let client = client();
    let send = capture_mixed(
        &client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &native,
            files: &files,
        })],
        0,
    )
    .expect("local frame");
    let header_len = u32::from_be_bytes(send.bytes()[8..12].try_into().expect("header")) as usize;
    let mut header: Value =
        serde_json::from_slice(&send.bytes()[HEADER_BYTES..HEADER_BYTES + header_len])
            .expect("metadata");
    header["sources"][0]["provenance"]["indexSha256"] = json!(vec![0u8; 32]);
    let header = serde_json::to_vec(&header).expect("forged metadata");
    let mut forged = MAGIC.to_vec();
    forged.extend_from_slice(&(header.len() as u32).to_be_bytes());
    forged.extend_from_slice(&header);
    forged.extend_from_slice(&send.bytes()[HEADER_BYTES + header_len..]);
    let digest = test_hash(&forged);
    assert!(
        restore(&client, &send, forged, digest).is_err(),
        "a local source cannot carry a queue index seal"
    );

    let mut unknown: Value = serde_json::from_slice(native.bytes()).expect("local JSON");
    unknown["item"]["files"][0]["blobId"] = json!(hex(&[3; 32]));
    assert!(
        QueueLocalCapture::from_bytes(
            serde_json::to_vec(&unknown).expect("unknown field"),
            DIRECTORY,
            test_hash
        )
        .is_err(),
        "local file schema refuses queue upload fields"
    );
}

#[test]
fn unsealed_sources_wrong_indexes_extra_bindings_and_duplicate_context_ids_refuse_capture() {
    let mut item = manifest();
    item["promptParts"] = json!({"context":[{"id":"msg_shared","text":"context"}]});
    let (doc, index) = sealed(item.clone(), &["msg_shared"], 100);
    let unsealed = QueuePayloadDocument::from_bytes(
        doc.bytes().to_vec(),
        DIRECTORY,
        QueuePayloadUse::Composer,
        test_hash,
    )
    .expect("parsed only");
    assert!(
        capture(&client(), &[source(&unsealed, &index, &[])], 0).is_err(),
        "parsing grants no index seal"
    );
    assert!(matches!(
        capture(
            &client(),
            &[source(&doc, &index, &[]), source(&doc, &index, &[])],
            0
        ),
        Err(QueueFrozenSendError::InvalidInput)
    ));
    let (other_doc, other_index) = sealed(manifest(), &[], 1);
    assert!(capture(&client(), &[source(&doc, &other_index, &[])], 0).is_err());
    let extra = [QueuePreparedFile::new(
        QueuePayloadFileSource::Legacy(0),
        0,
        test_hash(b""),
        "file:///tmp/owned/extra",
    )
    .expect("binding")];
    assert!(matches!(
        capture(&client(), &[source(&other_doc, &other_index, &extra)], 0),
        Err(QueueFrozenSendError::FileMismatch)
    ));
}

#[test]
fn frame_reconstruction_checks_digest_owner_config_lengths_and_embedded_request_identity() {
    let (doc, index) = sealed(manifest(), &[], 100);
    let client = client();
    let send = capture(&client, &[source(&doc, &index, &[])], 0).expect("capture");
    let mut bytes = send.bytes().to_vec();
    *bytes.last_mut().expect("body") ^= 1;
    assert!(matches!(
        restore(&client, &send, bytes, send.sha256()),
        Err(QueueFrozenSendError::DigestMismatch)
    ));
    let mut extra = send.bytes().to_vec();
    extra.push(b' ');
    let digest = test_hash(&extra);
    assert!(
        restore(&client, &send, extra, digest).is_err(),
        "trailing unframed bytes refuse"
    );
    let mut wrong_id = send.bytes().to_vec();
    let start = send.prompt_range.start;
    let position = wrong_id[start..]
        .windows(MESSAGE.len())
        .position(|bytes| bytes == MESSAGE.as_bytes())
        .expect("prompt ID");
    wrong_id[start + position + MESSAGE.len() - 1] = b'X';
    let digest = test_hash(&wrong_id);
    assert!(
        restore(&client, &send, wrong_id, digest).is_err(),
        "embedded request and metadata ID must match"
    );
    let wrong = ServerClient::new(
        Endpoint::parse("https://other.invalid/prefix/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    assert!(matches!(
        restore(&wrong, &send, send.bytes().to_vec(), send.sha256()),
        Err(QueueFrozenSendError::OwnerMismatch)
    ));
    let mut config = send.configuration();
    config.variant = None;
    assert!(
        QueueFrozenSend::from_bytes(
            &client,
            QueueFrozenSendOwner {
                session: &SessionId::new(SESSION),
                directory: DIRECTORY,
                message_id: &MessageId::new(MESSAGE),
                configuration: &config
            },
            send.bytes().to_vec(),
            send.sha256(),
            test_hash
        )
        .is_err()
    );
}

#[test]
fn captured_structured_and_actual_outgoing_budgets_are_independent() {
    let (doc, index) = sealed(manifest(), &[], QUEUE_PAYLOAD_BYTES as u64);
    assert!(
        capture(&client(), &[source(&doc, &index, &[])], 0).is_ok(),
        "50 MiB equality is accepted"
    );
    let (small, small_index) = sealed(manifest(), &[], 1);
    assert!(matches!(
        capture(
            &client(),
            &[source(&doc, &index, &[]), source(&small, &small_index, &[])],
            0
        ),
        Err(QueueFrozenSendError::StructuredTooLarge)
    ));
    let file = json!({"blobId":hex(&[3;32]),"requestId":"file_original","attachmentId":"file",
        "filename":"notes.txt","mimeType":"text/plain","size":2,"sha256":hex(&test_hash(b"hi"))});
    let mut item = manifest();
    item["attachmentManifest"] = Value::Array(vec![file; QUEUE_MANIFEST_ENTRIES]);
    let (aliases, aliases_index) = sealed(item, &[], 100_000);
    let uri = format!("file:///tmp/owned/{}", "a".repeat(40 * 1024));
    let bindings = [QueuePreparedFile::new(
        QueuePayloadFileSource::Manifest([3; 32]),
        2,
        test_hash(b"hi"),
        &uri,
    )
    .expect("binding")];
    assert!(
        matches!(
            capture(&client(), &[source(&aliases, &aliases_index, &bindings)], 0),
            Err(QueueFrozenSendError::OutgoingTooLarge)
        ),
        "aliased staged URIs are counted before frame allocation"
    );
    assert!(
        QueuePreparedFile::new(
            QueuePayloadFileSource::Legacy(0),
            QUEUE_BLOB_BYTES as u64 + 1,
            [0; 32],
            "file:///tmp/owned/large"
        )
        .is_err()
    );
    let mut count = Counter { bytes: 0, limit: 4 };
    assert!(
        io::Write::write_all(&mut count, b"1234").is_ok(),
        "exact byte limit is inclusive"
    );
    assert!(io::Write::write_all(&mut count, b"5").is_err());
    assert!(
        matches!(
            encode(&"x".repeat(QUEUE_MANIFEST_BYTES), QUEUE_MANIFEST_BYTES),
            Err(QueueFrozenSendError::OutgoingTooLarge)
        ),
        "JSON quotes count against the metadata limit"
    );
}

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    target: String,
    headers: String,
    body: Vec<u8>,
}
type Handler = dyn Fn(&Recorded) -> Option<(u16, String)> + Send + Sync;
struct Server {
    client: ServerClient,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(
        handler: impl Fn(&Recorded) -> Option<(u16, String)> + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
        let address = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handler: Arc<Handler> = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&recorded);
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    respond(socket, requests, handler).await;
                });
            }
        });
        let client = ServerClient::new(
            Endpoint::parse(&format!("http://{address}/prefix/")).expect("endpoint"),
            Credential::ClientToken("frozen-test-credential".to_owned()),
        )
        .expect("client");
        Self {
            client,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(
    mut socket: TcpStream,
    requests: Arc<Mutex<Vec<Recorded>>>,
    handler: Arc<Handler>,
) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.expect("request read");
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("headers");
    let mut first = headers
        .lines()
        .next()
        .expect("request line")
        .split_whitespace();
    let method = first.next().expect("method").to_owned();
    let target = first.next().expect("target").to_owned();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("length"))
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.expect("body read");
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let request = Recorded {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + length].to_vec(),
    };
    requests.lock().expect("request lock").push(request.clone());
    let Some((status, body)) = handler(&request) else {
        return;
    };
    let response = format!(
        "HTTP/1.1 {status} test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("response");
}

fn admitted(id: &str, kind: &str) -> String {
    json!({"data":{"id":id,"sessionID":SESSION,"type":kind,"time":{"created":5},"delivery":"steer",
        "payload":{"text":"plugin transformed content"}}})
    .to_string()
}

fn accept(request: &Recorded) -> Option<(u16, String)> {
    if request.target.ends_with("/model") || request.target.ends_with("/agent") {
        return Some((204, String::new()));
    }
    let body: Value = serde_json::from_slice(&request.body).expect("request JSON");
    Some((
        200,
        admitted(
            body["id"].as_str().expect("ID"),
            if request.target.ends_with("/prompt") {
                "user"
            } else {
                "synthetic"
            },
        ),
    ))
}

fn contextual(client: &ServerClient) -> QueueFrozenSend {
    let mut item = manifest();
    item["promptParts"] = json!({"context":[{"id":"msg_first_original","text":"first","description":"kept"},
        {"id":"msg_second_original","text":"second","metadata":{"kept":true}}]});
    let (doc, index) = sealed(item, &["msg_first_original", "msg_second_original"], 100);
    capture(client, &[source(&doc, &index, &[])], 0).expect("frozen context send")
}

#[tokio::test]
async fn local_send_restarts_and_dispatches_exact_saved_bytes_without_queue_requests() {
    let server = Server::start(accept).await;
    let native = native_capture();
    let files = native_files();
    let send = capture_mixed(
        &server.client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &native,
            files: &files,
        })],
        0,
    )
    .expect("local send");
    let saved_prompt = send.prompt_body().to_vec();
    let saved_context = send.context_body(0).expect("context").to_vec();
    let restored = restore(&server.client, &send, send.bytes().to_vec(), send.sha256())
        .expect("local restart");
    restored.dispatch(&[]).await.expect("explicit local send");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        4,
        "only model, agent, synthetic and prompt are dispatched"
    );
    assert_eq!(requests[2].body, saved_context);
    assert_eq!(requests[3].body, saved_prompt);
    assert!(
        requests.iter().all(|request| request
            .target
            .starts_with(&format!("/prefix/api/session/{SESSION}/"))),
        "ordinary send uses no message-queue route"
    );
    assert_eq!(
        restored.source_kinds().collect::<Vec<_>>(),
        [QueueFrozenSourceKind::Local]
    );
}

#[tokio::test]
async fn dispatch_preserves_body_bytes_auth_prefix_order_and_explicit_resume_ids() {
    for skip in [0, 1, 2] {
        let server = Server::start(accept).await;
        let send = contextual(&server.client);
        let receipt = send
            .dispatch(&send.context_ids()[..skip])
            .await
            .expect("admission");
        assert_eq!(receipt.message_id.as_str(), MESSAGE);
        let requests = server.requests();
        assert_eq!(
            requests.len(),
            5 - skip,
            "no reserve, prepare, or retry request"
        );
        assert!(requests[0].target.ends_with("/model"));
        assert!(requests[1].target.ends_with("/agent"));
        for (request, index) in requests[2..requests.len() - 1].iter().zip(skip..2) {
            assert!(request.target.ends_with("/synthetic"));
            assert_eq!(
                request.body,
                send.context_body(index).expect("retained context")
            );
        }
        assert_eq!(requests.last().expect("prompt").body, send.prompt_body());
        for request in requests {
            assert_eq!(request.method, "POST");
            assert!(
                request
                    .target
                    .starts_with(&format!("/prefix/api/session/{SESSION}/"))
            );
            let headers = request.headers.to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer frozen-test-credential"));
            assert!(
                !headers.contains("x-opencode-directory"),
                "real-session routes resolve their location"
            );
            assert!(headers.contains("content-type: application/json"));
        }
    }
}

#[tokio::test]
async fn single_step_dispatch_checks_context_order_and_emits_only_the_requested_body() {
    let server = Server::start(accept).await;
    let send = contextual(&server.client);
    let contexts = send.context_ids();
    for (step, admitted) in [
        (QueueFrozenDispatchStep::Prompt, &contexts[..0]),
        (QueueFrozenDispatchStep::Context(1), &contexts[..0]),
        (QueueFrozenDispatchStep::Context(0), &contexts[..1]),
        (QueueFrozenDispatchStep::Context(2), &contexts[..2]),
        (QueueFrozenDispatchStep::Model, &contexts[1..]),
    ] {
        let failure = send
            .dispatch_step(step, admitted)
            .await
            .expect_err("non-leading dispatch refuses before I/O");
        assert_eq!(failure.stage, SendStage::Validation);
    }
    assert!(
        server.requests().is_empty(),
        "invalid steps have no selection or admission authority"
    );
    assert!(
        send.dispatch_step(QueueFrozenDispatchStep::Model, &[])
            .await
            .expect("model selected")
            .is_none()
    );
    assert_eq!(
        server.requests().len(),
        1,
        "a model step cannot continue to agent or admission"
    );
    assert!(
        send.dispatch_step(QueueFrozenDispatchStep::Agent, &[])
            .await
            .expect("agent selected")
            .is_none()
    );
    for index in 0..contexts.len() {
        let receipt = send
            .dispatch_step(QueueFrozenDispatchStep::Context(index), &contexts[..index])
            .await
            .expect("next context admitted")
            .expect("context receipt");
        assert_eq!(receipt.message_id, contexts[index]);
        let requests = server.requests();
        assert_eq!(requests.len(), index + 3, "exactly one request per step");
        assert_eq!(
            requests.last().expect("context request").body,
            send.context_body(index).expect("saved context bytes")
        );
    }
    let receipt = send
        .dispatch_step(QueueFrozenDispatchStep::Prompt, &contexts)
        .await
        .expect("prompt admitted")
        .expect("prompt receipt");
    assert_eq!(receipt.message_id.as_str(), MESSAGE);
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        5,
        "no observation or queue requests are implicit"
    );
    assert_eq!(
        requests.last().expect("prompt request").body,
        send.prompt_body()
    );
}

#[tokio::test]
async fn single_step_failure_retains_prior_context_admission_and_absent_agent_refuses_locally() {
    let server = Server::start(|_| Some((401, "{}".to_owned()))).await;
    let send = contextual(&server.client);
    let failure = send
        .dispatch_step(QueueFrozenDispatchStep::Model, &send.context_ids()[..1])
        .await
        .expect_err("expired authentication");
    assert_eq!(failure.stage, SendStage::Model);
    assert_eq!(
        failure.admission,
        PromptAdmission::Partial,
        "selection failure cannot roll back a previously admitted context"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "the selection is attempted once"
    );
    let capture = QueueLocalCapture::new(
        DIRECTORY,
        QueueLocalItem {
            content: "display".to_owned(),
            text: "prompt".to_owned(),
            files: Vec::new(),
            send_config: QueuedSendConfig {
                provider_id: "provider".to_owned(),
                model_id: "model".to_owned(),
                agent: None,
                variant: None,
            },
            prompt_parts: QueuePromptParts::default(),
        },
        Vec::new(),
        test_hash,
    )
    .expect("agentless local capture");
    let send = capture_mixed(
        &server.client,
        &[QueueFrozenSource::Local(QueueLocalFrozenItem {
            capture: &capture,
            files: &[],
        })],
        0,
    )
    .expect("agentless send");
    let failure = send
        .dispatch_step(QueueFrozenDispatchStep::Agent, &[])
        .await
        .expect_err("no captured agent");
    assert_eq!(failure.stage, SendStage::Validation);
    assert_eq!(
        server.requests().len(),
        1,
        "no agent name can be inferred from current selection"
    );
}

#[tokio::test]
async fn admission_receipts_may_include_full_rich_payloads_larger_than_compact_queue_metadata() {
    let transformed = "x".repeat(QUEUE_MANIFEST_BYTES + 1);
    let server = Server::start(move |request| {
        if !request.target.ends_with("/prompt") {
            return accept(request);
        }
        Some((
            200,
            json!({"data":{"id":MESSAGE,"sessionID":SESSION,"type":"user","time":{"created":5},
            "delivery":"steer","payload":{"text":transformed}}})
            .to_string(),
        ))
    })
    .await;
    let send = contextual(&server.client);
    let receipt = send
        .dispatch(&[])
        .await
        .expect("full prepared payload does not invalidate matching admission identity");
    assert_eq!(receipt.message_id.as_str(), MESSAGE);
    assert_eq!(
        server.requests().len(),
        5,
        "the large receipt causes no retry"
    );
}

#[tokio::test]
async fn invalid_resume_rejects_before_selection_and_failures_retain_original_ids_without_retry() {
    let server = Server::start(accept).await;
    let send = contextual(&server.client);
    let failure = send
        .dispatch(&[MessageId::new("msg_second_original")])
        .await
        .expect_err("non-prefix");
    assert_eq!(failure.stage, SendStage::Validation);
    assert!(server.requests().is_empty());
    for (stage, status, expected) in [
        ("model", 401, PromptAdmission::NotAttempted),
        ("first", 400, PromptAdmission::NotAttempted),
        ("first", 409, PromptAdmission::Unknown),
        ("second", 403, PromptAdmission::Partial),
        ("second", 500, PromptAdmission::Unknown),
        ("prompt", 400, PromptAdmission::Partial),
        ("prompt", 0, PromptAdmission::Unknown),
    ] {
        let server = Server::start(move |request| {
            let fail = match stage {
                "first" => request
                    .body
                    .windows(b"msg_first_original".len())
                    .any(|bytes| bytes == b"msg_first_original"),
                "second" => request
                    .body
                    .windows(b"msg_second_original".len())
                    .any(|bytes| bytes == b"msg_second_original"),
                operation => request.target.ends_with(&format!("/{operation}")),
            };
            if fail {
                if status == 0 {
                    None
                } else {
                    Some((status, "{}".to_owned()))
                }
            } else {
                accept(request)
            }
        })
        .await;
        let send = contextual(&server.client);
        let failure = send.dispatch(&[]).await.expect_err("scripted failure");
        assert_eq!(failure.admission, expected, "{stage} HTTP {status}");
        assert_eq!(failure.session_id.as_str(), SESSION);
        assert_eq!(failure.message_id.as_str(), MESSAGE);
        let requests = server.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| match stage {
                    "first" => request
                        .body
                        .windows(b"msg_first_original".len())
                        .any(|bytes| bytes == b"msg_first_original"),
                    "second" => request
                        .body
                        .windows(b"msg_second_original".len())
                        .any(|bytes| bytes == b"msg_second_original"),
                    operation => request.target.ends_with(&format!("/{operation}")),
                })
                .count(),
            1,
            "failed requests are never automatically replayed"
        );
    }
}

#[tokio::test]
async fn observation_is_read_only_and_preserves_missing_out_of_order_pending_and_failure() {
    for case in ["leading", "gap", "pending", "wrong-owner", "failure"] {
        let server = Server::start(move |request| {
            if !request.target.ends_with("/inbox") { return Some((404, "{}".to_owned())); }
            if case == "failure" { return Some((500, "{}".to_owned())); }
            let (id, kind) = match case { "pending" | "wrong-owner" => (MESSAGE, "user"), "gap" => ("msg_second_original", "synthetic"), _ => ("msg_first_original", "synthetic") };
            Some((200, json!({"data":[{"id":id,"sessionID":if case == "wrong-owner" { "ses_other" } else { SESSION },
                "type":kind,"time":{"created":5},"delivery":"steer"}]}).to_string()))
        }).await;
        let send = contextual(&server.client);
        let observed = send.observe().await;
        match case {
            "leading" => assert_eq!(
                observed.expect("observation"),
                SendObservation::Missing {
                    admitted: vec![MessageId::new("msg_first_original")]
                }
            ),
            "gap" => assert_eq!(observed.expect("observation"), SendObservation::OutOfOrder),
            "pending" => assert!(matches!(observed, Ok(SendObservation::Pending(_)))),
            _ => assert!(observed.is_err(), "failed reads grant no absence authority"),
        }
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.method == "GET" && request.body.is_empty())
        );
    }
}
