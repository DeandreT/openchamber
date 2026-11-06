//! Scripted loopback contracts for durable manifests, ownership and raw bytes.

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::credential::Credential;

use super::*;

const DIRECTORY: &str = "/home/synthetic/my project/ü?x=1&y=%2F";
const REQUEST: &str = "request_original_1";
const SHA: [u8; 32] = [0xab; 32];

fn make_client(base: &str) -> ServerClient {
    ServerClient::new(
        Endpoint::parse(base).expect("fixture endpoint"),
        Credential::ClientToken("oc_client_queue_fixture".to_owned()),
    )
    .expect("fixture client")
}

fn scope(client: &ServerClient) -> QueueScope {
    QueueScope::new(client, SessionId::new("ses_fixture"), DIRECTORY).expect("fixture owner")
}

fn owner(client: &ServerClient) -> QueueRequestOwner {
    scope(client)
        .request(REQUEST)
        .expect("fixture request identity")
}

fn config_fixture() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider_fixture".to_owned(),
        model_id: "model_fixture".to_owned(),
        agent: Some("build".to_owned()),
        variant: Some("high".to_owned()),
    }
}

fn file(id: &str) -> QueueManifestAttachment {
    QueueManifestAttachment {
        attachment: QueueAttachmentDescriptor {
            blob_id: [0xcd; 32],
            request_id: "original_upload_1".to_owned(),
            attachment_id: id.to_owned(),
            filename: "private ü image.png".to_owned(),
            mime_type: "image/png".to_owned(),
            size: QUEUE_BLOB_BYTES as u64,
            sha256: SHA,
        },
        agent_path: None,
        description: Some("private attachment description".to_owned()),
        mention: Some(QueueMention {
            start: 1,
            end: 8,
            text: "@image".to_owned(),
        }),
    }
}

fn item() -> QueueManifestItem {
    QueueManifestItem {
        content: "private display text".to_owned(),
        text: "private delivered text".to_owned(),
        attachment_manifest: vec![file("image_1"), file("image_2")],
        context: vec![
            QueueLegacyContext::Context {
                text: "private legacy context".to_owned(),
                metadata: Metadata(Map::from_iter([(
                    "legacy".to_owned(),
                    json!({"nested":[true, null, 1.5]}),
                )])),
                instructions: Some("private instructions".to_owned()),
            },
            QueueLegacyContext::Instruction {
                text: "private skill instruction".to_owned(),
            },
            QueueLegacyContext::Synthetic {
                text: "private synthetic text".to_owned(),
            },
        ],
        context_preview: Some("private preview".into()),
        send_config: config_fixture(),
        prompt_parts: QueuePromptParts {
            file_references: vec![],
            agents: vec![
                QueueAgent {
                    name: "plan".to_owned(),
                    mention: Some(QueueMention {
                        start: 0,
                        end: 5,
                        text: "@plan".to_owned(),
                    }),
                },
                QueueAgent {
                    name: "build".to_owned(),
                    mention: None,
                },
            ],
            skills: vec![
                QueueSkill {
                    id: "skill_fixture".to_owned(),
                    mention: Some(QueueMention {
                        start: 8,
                        end: 14,
                        text: "@skill".to_owned(),
                    }),
                },
                QueueSkill {
                    id: "skill_other".to_owned(),
                    mention: None,
                },
            ],
            metadata: Some(Metadata(Map::from_iter([(
                "private_metadata".to_owned(),
                json!({"value":["private value",false]}),
            )]))),
            context: vec![
                QueuePromptContext {
                    id: Some(MessageId::new("msg_explicit")),
                    text: "private context 1".to_owned(),
                    description: Some(String::new()),
                    metadata: Some(Metadata(Map::new())),
                },
                QueuePromptContext {
                    id: None,
                    text: "private context 2".to_owned(),
                    description: None,
                    metadata: None,
                },
            ],
        },
    }
}

fn contexts() -> Vec<String> {
    [
        "msg_instructions",
        "msg_legacy",
        "msg_instruction",
        "msg_synthetic",
        "msg_explicit",
        "msg_allocated",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[test]
fn reference_parts_round_trip_enqueue_and_prefix_and_default_bodies_omit_the_new_field() {
    let client = make_client("https://example.invalid/prefix/");
    let unchanged = QueueEnqueue::new(owner(&client), item()).expect("existing capture");
    assert!(!String::from_utf8_lossy(unchanged.body()).contains("fileReferences"));
    let reference = QueueFileReference {
        uri: "file:///home/synthetic/report.pdf".to_owned(),
        name: "report.pdf".to_owned(),
        description: Some("original PDF".to_owned()),
        mention: Some(QueueMention {
            start: 1,
            end: 8,
            text: "@report".to_owned(),
        }),
    };
    let mut captured = item();
    captured.prompt_parts.file_references = vec![reference.clone(), reference.clone()];
    let enqueue = QueueEnqueue::new(owner(&client), captured.clone())
        .expect("explicit references in enqueue");
    assert_eq!(
        QueueEnqueue::from_body(owner(&client), enqueue.body().to_vec())
            .expect("enqueue restart")
            .item(),
        &captured
    );
    let mut input = prefix();
    input
        .prompt_parts
        .as_mut()
        .expect("composer parts")
        .file_references = vec![reference];
    let prefix = QueuePrefixCapture::new(scope(&client), input.clone())
        .expect("references in prefix composer");
    let restored = QueuePrefixCapture::from_body(scope(&client), prefix.body().to_vec())
        .expect("prefix restart");
    assert_eq!(restored.input(), &input);
    assert_eq!(restored.body(), prefix.body());
}

fn taken_json(id: &str) -> Value {
    let item = item();
    let mut value = item_value(&item);
    if let Some(preview) = &item.context_preview {
        value["contextPreview"] = json!(preview.display());
    }
    value["id"] = json!(id);
    value["createdAt"] = json!(17);
    value["requestId"] = json!("source_enqueue_1");
    value["attachments"] = json!([]);
    value
}

fn enqueue_json() -> Value {
    json!({"receipt": {"version":1,"kind":"enqueue", "requestId":REQUEST,"sessionId":"ses_fixture",
        "directory":DIRECTORY,"requestHash":hex_digest(&[0xef;32]),"acceptedAt":15,
        "itemId":"queued_a","messageId":"msg_server_original","contextIds":contexts()},
        "state":"queued","attempt":0,"revision":19})
}

fn take_json(acknowledged: bool) -> Value {
    let mut value = json!({"receipt": {"version":1,"kind":"take", "requestId":REQUEST,"sessionId":"ses_fixture",
        "directory":DIRECTORY,"requestHash":hex_digest(&[0xef;32]),"acceptedAt":15,"itemIds":["queued_a"]},
        "state":if acknowledged {"acknowledged"} else {"transferring"},"revision":19});
    if !acknowledged {
        value["items"] = json!([taken_json("queued_a")]);
    }
    value
}

#[test]
fn manifest_take_and_lookup_parser_preserves_split_context_preview_units() {
    let client = make_client("https://example.invalid/prefix/");
    let owner = owner(&client);
    let bytes = serde_json::to_string(&take_json(false))
        .expect("complete transfer reply")
        .replace("private preview", r"\ud83d...");
    let QueueRecord::Take(record) =
        parse_record(bytes.as_bytes(), &owner).expect("typed manifest receipt parser")
    else {
        panic!("take receipt");
    };
    let QueueTakeState::Transferring { items } = record.state else {
        panic!("transferring take");
    };
    assert_eq!(
        items[0]
            .item
            .context_preview
            .as_ref()
            .expect("context preview")
            .utf16(),
        &[0xd83d, 0x2e, 0x2e, 0x2e],
        "submit and request lookup share this exact record parser"
    );
}

fn upload_json() -> Value {
    json!({"attachment": {"blobId":hex_digest(&[0xcd;32]), "requestId":REQUEST,"attachmentId":"image_1",
        "filename":"name ü %.png","mimeType":"image/png","size":3,"sha256":hex_digest(&SHA)},"state":"available"})
}

fn prefix() -> QueuePrefixInput {
    QueuePrefixInput {
        id: MessageId::new("msg_prefix_original"),
        item_ids: vec![QueuedItemId::new("queued_a")],
        composer: "private composer".to_owned(),
        send_config: config_fixture(),
        steer: true,
        prompt_parts: Some(QueuePromptParts {
            context: vec![QueuePromptContext {
                id: Some(MessageId::new("msg_composer")),
                text: "private composer context".to_owned(),
                description: None,
                metadata: None,
            }],
            ..QueuePromptParts::default()
        }),
    }
}

fn prefix_json(capture: &QueuePrefixCapture) -> Value {
    let mut ids = contexts();
    ids.push("msg_composer".to_owned());
    json!({"revision":21,"reservation":serde_json::from_slice::<Value>(capture.body()).expect("captured body"),
        "items":[taken_json("queued_a")],"contextIds":ids})
}

fn captured_numeric_metadata() -> Metadata {
    Metadata(Map::from_iter([
        ("one".to_owned(), json!(1.0)),
        ("nested".to_owned(), json!([{"n":1.0,"zero":-0.0}])),
        ("large".to_owned(), json!(1.0000000000000001e18_f64)),
    ]))
}

fn javascript_numeric_metadata() -> Map<String, Value> {
    // JSON.stringify emits integer spellings for integral Number values and
    // emits 0 for -0. The large decimal spelling round-trips to the same f64.
    serde_json::from_str(r#"{"one":1,"nested":[{"n":1,"zero":0}],"large":1000000000000000100}"#)
        .expect("JavaScript-normalized JSON fixture")
}

#[test]
fn captures_round_trip_all_prompt_parts_without_inline_bytes_or_domain_loss() {
    let client = make_client("https://example.invalid/prefix/");
    let capture = QueueEnqueue::new(owner(&client), item()).expect("manifest capture");
    let body: Value = serde_json::from_slice(capture.body()).expect("manifest JSON");
    assert!(
        capture.body().len() < 10_000,
        "two 20 MiB descriptors do not expand into JSON bytes"
    );
    assert_eq!(body["item"]["format"], "manifest-v1");
    assert!(body["item"].get("attachments").is_none());
    assert_eq!(
        body["item"]["promptParts"]["skills"][0]["mention"]["text"],
        "@skill"
    );
    assert_eq!(body["item"]["promptParts"]["context"][0]["description"], "");
    assert_eq!(
        body["item"]["promptParts"]["context"][0]["metadata"],
        json!({})
    );
    assert_eq!(
        body["item"]["attachmentManifest"][0]["requestId"],
        "original_upload_1"
    );
    let restored =
        QueueEnqueue::from_body(owner(&client), capture.body().to_vec()).expect("retained body");
    assert_eq!(restored.body(), capture.body());
    assert_eq!(restored.item(), capture.item());
    let mut taken = taken_json("queued_a");
    let decoded = taken_items(vec![serde_json::from_value(taken.clone()).expect("wire")])
        .expect("full manifest");
    assert_eq!(decoded[0].item, item());
    assert_eq!(decoded[0].request_id.as_deref(), Some("source_enqueue_1"));
    taken["attachments"] = json!([{"dataUrl":"data:bytes"}]);
    assert!(taken_items(vec![serde_json::from_value(taken).expect("wire")]).is_err());
}

#[test]
fn manifest_model_identities_preserve_literal_names_omission_and_exact_recovery_bytes() {
    let client = make_client("https://example.invalid/prefix/");
    let original = QueueEnqueue::new(owner(&client), item()).expect("existing manifest");
    assert!(
        !String::from_utf8_lossy(original.body()).contains("agentPath"),
        "omission adds no default field"
    );
    let names = [
        "relative/../source.txt".to_owned(),
        r"C:\original folder\source.txt".to_owned(),
        r"\\server\share\source.txt".to_owned(),
        "/posix/literal\\name?#%2F.txt".to_owned(),
        " \tunchanged name\t ".to_owned(),
        format!("/model/{}.txt", "a".repeat(5000)),
    ];
    for name in names {
        let mut source = item();
        source.attachment_manifest[0].agent_path = Some(name.clone());
        source.attachment_manifest[1].attachment = source.attachment_manifest[0].attachment.clone();
        source.attachment_manifest[1].agent_path = Some("another relative identity".to_owned());
        let capture = QueueEnqueue::new(owner(&client), source.clone())
            .expect("literal prepared-file identities");
        let value: Value = serde_json::from_slice(capture.body()).expect("captured JSON");
        assert_eq!(value["item"]["attachmentManifest"][0]["agentPath"], name);
        assert_eq!(
            value["item"]["attachmentManifest"][0]["filename"],
            "private ü image.png"
        );
        let bytes = format!(
            " \n{}\n",
            std::str::from_utf8(capture.body()).expect("UTF-8 capture")
        )
        .into_bytes();
        let restored =
            QueueEnqueue::from_body(owner(&client), bytes.clone()).expect("exact restart");
        assert_eq!(
            restored.body(),
            bytes,
            "recovery never inserts, removes or normalizes fields"
        );
        assert_eq!(restored.item(), &source);
        let mut reply = take_json(false);
        reply["items"][0]["attachmentManifest"] = value["item"]["attachmentManifest"].clone();
        let QueueRecord::Take(record) = parse_record(
            &serde_json::to_vec(&reply).expect("transfer JSON"),
            &owner(&client),
        )
        .expect("complete manifest transfer") else {
            panic!("take receipt");
        };
        let QueueTakeState::Transferring { items } = record.state else {
            panic!("transferring take");
        };
        assert_eq!(
            items[0].item.attachment_manifest,
            source.attachment_manifest
        );
        assert!(
            !format!("{capture:?} {:?}", source.attachment_manifest).contains(name.as_str()),
            "model identities stay redacted"
        );
    }
}

#[test]
fn malformed_manifest_model_identities_fail_input_and_complete_transfer_parsing() {
    let client = make_client("https://example.invalid/prefix/");
    for malformed in [
        Value::Null,
        json!(""),
        json!(42),
        json!(true),
        json!([]),
        json!({}),
    ] {
        let mut input: Value = serde_json::from_slice(
            QueueEnqueue::new(owner(&client), item())
                .expect("valid input")
                .body(),
        )
        .expect("input JSON");
        input["item"]["attachmentManifest"][0]["agentPath"] = malformed.clone();
        assert!(
            QueueEnqueue::from_body(
                owner(&client),
                serde_json::to_vec(&input).expect("malformed input")
            )
            .is_err(),
            "a present invalid identity cannot become a basename fallback"
        );
        let mut reply = take_json(false);
        reply["items"][0]["attachmentManifest"][0]["agentPath"] = malformed;
        assert!(
            parse_record(
                &serde_json::to_vec(&reply).expect("malformed reply"),
                &owner(&client)
            )
            .is_err(),
            "no partial transfer can acquire ownership after an invalid identity"
        );
    }
    let mut source = item();
    source.attachment_manifest[0].agent_path = Some(String::new());
    assert!(
        QueueEnqueue::new(owner(&client), source).is_err(),
        "typed constructors reject empty identities too"
    );
    let mut upload = upload_json();
    upload["attachment"]["agentPath"] = json!("/not/upload/metadata");
    assert!(
        serde_json::from_value::<AttachmentReplyWire>(upload).is_err(),
        "upload descriptors remain strict and path-free"
    );
}

#[test]
fn restore_retains_original_omission_and_byte_spelling() {
    let client = make_client("https://example.invalid");
    let body = format!("{{ \"directory\":{}, \"item\":{{\"format\":\"manifest-v1\",\"content\":\"\",\"text\":\"hi\",\"attachmentManifest\":[],\"sendConfig\":{{\"providerID\":\"p\",\"modelID\":\"m\"}}}} }}",
        serde_json::to_string(DIRECTORY).expect("directory JSON")).into_bytes();
    let capture = QueueEnqueue::from_body(owner(&client), body.clone())
        .expect("omitted empty arrays are supported");
    assert_eq!(capture.body(), body);
    assert_eq!(capture.item().prompt_parts, QueuePromptParts::default());
    let mut value: Value = serde_json::from_slice(&body).expect("JSON");
    for field in [
        "id",
        "createdAt",
        "requestId",
        "attachments",
        "goal",
        "scheduledTask",
        "extra",
    ] {
        value["item"][field] = json!(null);
        assert!(
            QueueEnqueue::from_body(owner(&client), serde_json::to_vec(&value).expect("JSON"))
                .is_err(),
            "reject {field}"
        );
        value["item"].as_object_mut().expect("item").remove(field);
    }
    for field in ["context", "promptParts", "contextPreview"] {
        value["item"][field] = json!(null);
        assert!(
            QueueEnqueue::from_body(owner(&client), serde_json::to_vec(&value).expect("JSON"))
                .is_err()
        );
        value["item"].as_object_mut().expect("item").remove(field);
    }
}

#[test]
fn raw_upload_request_preserves_bytes_headers_prefix_query_and_captured_client() {
    let client = make_client("https://example.invalid/reverse/prefix/");
    let upload = QueueUpload::new(
        owner(&client),
        "image_1",
        "name ü %.png",
        "image/png",
        SHA,
        vec![0, 255, 7],
    )
    .expect("upload capture");
    let built = upload.request().build().expect("authenticated request");
    assert_eq!(built.method(), Method::PUT);
    assert_eq!(
        built.url().path(),
        "/reverse/prefix/api/message-queue/sessions/ses_fixture/requests/request_original_1/attachments/image_1"
    );
    assert_eq!(
        built.url().query_pairs().collect::<Vec<_>>(),
        vec![("directory".into(), DIRECTORY.into())]
    );
    assert_eq!(
        built.headers()["authorization"],
        "Bearer oc_client_queue_fixture"
    );
    assert_eq!(
        built.headers()["x-openchamber-filename"],
        "name%20%C3%BC%20%25.png"
    );
    assert_eq!(built.headers()["x-openchamber-mime-type"], "image/png");
    assert_eq!(built.headers()["x-openchamber-size"], "3");
    assert_eq!(built.headers()["x-openchamber-sha256"], hex_digest(&SHA));
    assert_eq!(built.headers()["content-type"], "application/octet-stream");
    assert!(
        built.headers().get("x-opencode-directory").is_none(),
        "host directory stays in the exact query"
    );
    assert_eq!(
        built.body().expect("binary body").as_bytes(),
        Some([0, 255, 7].as_slice())
    );
    let other = make_client("https://different.invalid/");
    drop(other);
    assert_eq!(
        upload.owner().scope().endpoint().as_str(),
        "https://example.invalid/reverse/prefix/"
    );
}

#[test]
fn validation_matches_backend_filename_mime_ids_and_20_mib_inclusive_limit() {
    let client = make_client("https://example.invalid/");
    for name in [" ", "a:b?.txt", "CON", "ü%.png", ".env"] {
        assert!(
            QueueUpload::new(owner(&client), "a", name, "text/plain", SHA, vec![]).is_ok(),
            "display metadata is not a filesystem path"
        );
    }
    for name in ["", ".", "..", "../x", "x\\y", "x\n", "x\u{85}"] {
        assert!(QueueUpload::new(owner(&client), "a", name, "text/plain", SHA, vec![]).is_err());
    }
    for mime in [
        "text",
        "image/png; charset=utf-8",
        "text/plain/extra",
        "image/",
        "/png",
        "text/pla in",
    ] {
        assert!(QueueUpload::new(owner(&client), "a", "x", mime, SHA, vec![]).is_err());
    }
    for id in ["", "a/b", "a?x", "ü"] {
        assert!(QueueUpload::new(owner(&client), id, "x", "text/plain", SHA, vec![]).is_err());
    }
    let exact = QueueUpload::new(
        owner(&client),
        "a",
        "x",
        "text/plain",
        SHA,
        vec![0; QUEUE_BLOB_BYTES],
    )
    .expect("inclusive bound");
    assert_eq!(exact.bytes().len(), QUEUE_BLOB_BYTES);
    assert!(matches!(
        QueueUpload::new(
            owner(&client),
            "a",
            "x",
            "text/plain",
            SHA,
            vec![0; QUEUE_BLOB_BYTES + 1]
        ),
        Err(QueueManifestError::BlobTooLarge)
    ));
    for directory in ["", " /repo", "/repo ", "\u{feff}/repo"] {
        assert!(QueueScope::new(&client, SessionId::new("ses_fixture"), directory).is_err());
    }
    let windows = "C:\\exact project\\";
    assert_eq!(
        QueueScope::new(&client, SessionId::new("ses_fixture"), windows)
            .expect("exact Windows owner")
            .directory(),
        windows
    );
    assert!(scope(&client).request("short").is_err());
    assert!(scope(&client).request("request/escape").is_err());
}

#[test]
fn space_ownership_uses_marked_directory_header_and_original_upload_identity() {
    let client = make_client("https://example.invalid/prefix/");
    let directory = "/spaces/0123456789ab/my project";
    let scope =
        QueueScope::new(&client, SessionId::new("ses_fixture"), directory).expect("space owner");
    let handle = QueueAttachmentHandle::new(scope, file("image_1").attachment).expect("descriptor");
    for method in [Method::GET, Method::DELETE] {
        let request = handle.request(method.clone()).build().expect("request");
        assert_eq!(request.method(), method);
        assert_eq!(
            request.headers()["x-opencode-directory"],
            encode_directory(directory)
        );
        assert_eq!(request.headers()["x-opencode-directory-encoding"], "uri");
        assert!(request.body().is_none());
        assert!(
            request
                .url()
                .path()
                .contains("/requests/original_upload_1/attachments/image_1")
        );
        assert_eq!(request.url().query_pairs().count(), 1);
    }
}

#[test]
fn invalid_complete_receipts_never_become_empty_or_partial_success() {
    let client = make_client("https://example.invalid/");
    let owner = owner(&client);
    let parse = |value: &Value| parse_record(&serde_json::to_vec(value).expect("JSON"), &owner);
    assert!(matches!(
        parse(&enqueue_json()),
        Ok(QueueRecord::Enqueue(_))
    ));
    assert!(matches!(parse(&take_json(false)), Ok(QueueRecord::Take(_))));
    for field in ["requestId", "sessionId", "directory"] {
        let mut value = enqueue_json();
        value["receipt"][field] = json!("wrong_owner");
        assert!(parse(&value).is_err(), "reject wrong {field}");
    }
    for digest in [
        String::new(),
        "AB".repeat(32),
        "g".repeat(64),
        "ab".to_owned(),
    ] {
        let mut value = enqueue_json();
        value["receipt"]["requestHash"] = json!(digest);
        assert!(parse(&value).is_err(), "lowercase SHA-256 is required");
    }
    for field in ["acceptedAt", "version"] {
        let mut value = enqueue_json();
        value["receipt"][field] = json!(-1);
        assert!(parse(&value).is_err());
    }
    let mut unsafe_revision = enqueue_json();
    unsafe_revision["revision"] = json!(MAX_SAFE_INTEGER + 1);
    assert!(parse(&unsafe_revision).is_err());
    let mut same_id = enqueue_json();
    same_id["receipt"]["contextIds"][0] = same_id["receipt"]["messageId"].clone();
    assert!(parse(&same_id).is_err());
    let mut duplicate = enqueue_json();
    duplicate["receipt"]["contextIds"][1] = duplicate["receipt"]["contextIds"][0].clone();
    assert!(parse(&duplicate).is_err());
    for state in ["", "sending", "unknown"] {
        let mut value = enqueue_json();
        value["state"] = json!(state);
        assert!(parse(&value).is_err());
    }
    let mut growing_queue = enqueue_json();
    growing_queue["session"] = json!({"items":[]});
    assert!(
        parse(&growing_queue).is_err(),
        "receipt route has no queue projection"
    );
    let mut missing = take_json(false);
    missing.as_object_mut().expect("record").remove("items");
    assert!(parse(&missing).is_err());
    let mut acknowledged_payload = take_json(true);
    acknowledged_payload["items"] = json!([]);
    assert!(parse(&acknowledged_payload).is_err());
    let mut wrong_order = take_json(false);
    wrong_order["items"][0]["id"] = json!("queued_other");
    assert!(parse(&wrong_order).is_err());
    let mut malformed_sibling = take_json(false);
    malformed_sibling["receipt"]["itemIds"] = json!(["queued_a", "queued_b"]);
    malformed_sibling["items"] = json!([taken_json("queued_a"), {}]);
    assert!(
        parse(&malformed_sibling).is_err(),
        "full ownership cannot use a partial payload"
    );
}

#[test]
fn manifest_entry_and_metadata_limits_cover_combined_handoffs() {
    let client = make_client("https://example.invalid/");
    let mut input = item();
    input.prompt_parts.skills = (0..QUEUE_MANIFEST_ENTRIES)
        .map(|_| QueueSkill {
            id: "s".to_owned(),
            mention: None,
        })
        .collect();
    assert!(QueueEnqueue::new(owner(&client), input).is_err());
    let mut a = taken_json("queued_a");
    let mut b = taken_json("queued_b");
    for value in [&mut a, &mut b] {
        value["attachmentManifest"] = json!([]);
        value["context"] = json!([]);
        value["promptParts"] = json!({"agents":[],"skills":(0..2049).map(|_| json!({"id":"s"})).collect::<Vec<_>>(),"context":[]});
    }
    assert!(
        taken_items(vec![
            serde_json::from_value(a).expect("wire"),
            serde_json::from_value(b).expect("wire")
        ])
        .is_err(),
        "combined take exceeds 4096 even when each item fits"
    );
    let mut large = item();
    large.text = "x".repeat(QUEUE_MANIFEST_BYTES);
    assert!(matches!(
        QueueEnqueue::new(owner(&client), large),
        Err(QueueManifestError::MetadataTooLarge)
    ));
    let mut text = item();
    text.text = "x".repeat(1048576 + 1);
    assert!(
        QueueEnqueue::new(owner(&client), text).is_ok(),
        "manifest metadata uses 4 MiB, not the legacy 1 MiB text bound"
    );
    let mut wide_mention = item();
    wide_mention.prompt_parts.skills[0]
        .mention
        .as_mut()
        .expect("mention")
        .end = u64::from(u32::MAX) + 1;
    let capture = QueueEnqueue::new(owner(&client), wide_mention)
        .expect("safe JS offsets remain representable");
    assert_eq!(
        capture.item().prompt_parts.skills[0]
            .mention
            .as_ref()
            .expect("mention")
            .end,
        u64::from(u32::MAX) + 1
    );
}

#[test]
fn debug_output_omits_text_names_metadata_digests_paths_and_binary_content() {
    let client = make_client("https://example.invalid/");
    let enqueue = QueueEnqueue::new(owner(&client), item()).expect("capture");
    let upload = QueueUpload::new(
        owner(&client),
        "image_1",
        "name ü %.png",
        "image/png",
        SHA,
        vec![0, 255, 7],
    )
    .expect("upload");
    let debug = format!(
        "{enqueue:?} {:?} {upload:?} {:?} {:?}",
        item(),
        file("image_1"),
        prefix()
    );
    for private in [
        "private",
        "name ü",
        "oc_client_queue_fixture",
        "original_upload_1",
        DIRECTORY,
        "abababab",
        "@skill",
        "@plan",
    ] {
        assert!(!debug.contains(private), "debug leaked fixture content");
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    bytes: Vec<u8>,
    chunked: bool,
}

impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            headers: vec![],
            bytes: serde_json::to_vec(&value).expect("fixture JSON"),
            chunked: false,
        }
    }

    fn error(status: u16, code: Option<&str>) -> Self {
        let mut reply = Self::json(match code {
            Some(code) => json!({"code":code,"error":"private server text"}),
            None => json!({"error":"private server text"}),
        });
        reply.status = status;
        reply
    }

    fn binary(bytes: Vec<u8>, digest: &str, chunked: bool) -> Self {
        Self {
            status: 200,
            content_type: "application/octet-stream",
            headers: vec![("X-OpenChamber-SHA256".to_owned(), digest.to_owned())],
            bytes,
            chunked,
        }
    }
}

struct Recorded {
    head: String,
    body: Vec<u8>,
}

async fn read_request(socket: &mut TcpStream) -> Recorded {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 8192];
        let count = socket.read(&mut chunk).await.expect("request read");
        assert!(count > 0, "request header must complete");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 65536, "bounded fixture header");
    };
    let head = String::from_utf8(bytes[..end].to_vec()).expect("ASCII request headers");
    let length = head
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("decimal content length")
                })
        })
        .unwrap_or(0);
    assert!(length <= QUEUE_BLOB_BYTES, "bounded request body");
    let mut body = bytes[end..].to_vec();
    while body.len() < length {
        let mut chunk = [0; 8192];
        let count = socket.read(&mut chunk).await.expect("request body");
        assert!(count > 0, "request bytes must complete");
        body.extend_from_slice(&chunk[..count]);
    }
    assert_eq!(body.len(), length);
    Recorded { head, body }
}

async fn serve(replies: Vec<Reply>) -> (ServerClient, tokio::task::JoinHandle<Vec<Recorded>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let port = listener.local_addr().expect("listener address").port();
    let task = tokio::spawn(async move {
        let mut records = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.expect("fixture connection");
            records.push(read_request(&mut socket).await);
            let framing = if reply.chunked {
                "Transfer-Encoding: chunked\r\n".to_owned()
            } else {
                format!("Content-Length: {}\r\n", reply.bytes.len())
            };
            let mut head = format!(
                "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\n{}Connection: close\r\n",
                reply.status, reply.content_type, framing
            );
            for (name, value) in reply.headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            if socket.write_all(head.as_bytes()).await.is_ok() {
                if reply.chunked {
                    // Bounded chunks exercise the client stream limit even when
                    // Content-Length cannot reject an oversized answer first.
                    for chunk in reply.bytes.chunks(8192) {
                        if socket
                            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                            .await
                            .is_err()
                            || socket.write_all(chunk).await.is_err()
                            || socket.write_all(b"\r\n").await.is_err()
                        {
                            break;
                        }
                    }
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                } else {
                    let _ = socket.write_all(&reply.bytes).await;
                }
            }
            let _ = socket.shutdown().await;
        }
        records
    });
    (
        make_client(&format!("http://127.0.0.1:{port}/reverse/")),
        task,
    )
}

#[tokio::test]
async fn scripted_upload_validates_echo_and_preserves_exact_binary_bytes() {
    let (client, task) = serve(vec![Reply::json(upload_json())]).await;
    let upload = QueueUpload::new(
        owner(&client),
        "image_1",
        "name ü %.png",
        "image/png",
        SHA,
        vec![0, 255, 7],
    )
    .expect("capture");
    let result = upload.submit().await.expect("accepted upload");
    assert_eq!(result.request_id, REQUEST);
    let records = task.await.expect("fixture task");
    assert_eq!(records.len(), 1, "one explicit attempt");
    assert_eq!(records[0].body, vec![0, 255, 7]);
    assert!(records[0].head.starts_with("PUT /reverse/api/message-queue/sessions/ses_fixture/requests/request_original_1/attachments/image_1?directory="));
    assert!(
        records[0]
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer oc_client_queue_fixture\r\n")
    );
    for field in [
        "requestId",
        "attachmentId",
        "filename",
        "mimeType",
        "sha256",
        "blobId",
        "size",
    ] {
        let mut reply = upload_json();
        reply["attachment"][field] = if field == "size" {
            json!(4)
        } else {
            json!("wrong")
        };
        let (client, task) = serve(vec![Reply::json(reply)]).await;
        let upload = QueueUpload::new(
            owner(&client),
            "image_1",
            "name ü %.png",
            "image/png",
            SHA,
            vec![0, 255, 7],
        )
        .expect("capture");
        assert!(
            upload.submit().await.is_err(),
            "reject wrong upload {field}"
        );
        assert_eq!(task.await.expect("fixture").len(), 1);
    }
}

#[tokio::test]
async fn two_20_mib_uploads_keep_independent_raw_bodies_and_small_manifests() {
    let mut first_reply = upload_json();
    first_reply["attachment"]["size"] = json!(QUEUE_BLOB_BYTES);
    let mut second_reply = first_reply.clone();
    second_reply["attachment"]["attachmentId"] = json!("image_2");
    let (client, task) = serve(vec![Reply::json(first_reply), Reply::json(second_reply)]).await;
    let first = QueueUpload::new(
        owner(&client),
        "image_1",
        "name ü %.png",
        "image/png",
        SHA,
        vec![0xff; QUEUE_BLOB_BYTES],
    )
    .expect("first prepared file");
    let second = QueueUpload::new(
        owner(&client),
        "image_2",
        "name ü %.png",
        "image/png",
        SHA,
        vec![0; QUEUE_BLOB_BYTES],
    )
    .expect("second prepared file");
    let a = first.submit().await.expect("first independent upload");
    let b = second.submit().await.expect("second independent upload");
    assert!(
        matches!(
            QueueUpload::new(
                owner(&client),
                "image_3",
                "x.png",
                "image/png",
                SHA,
                vec![0; QUEUE_BLOB_BYTES + 1]
            ),
            Err(QueueManifestError::BlobTooLarge)
        ),
        "reject before another request"
    );
    let mut manifest = item();
    manifest.attachment_manifest = [a, b]
        .into_iter()
        .map(|attachment| QueueManifestAttachment {
            attachment,
            agent_path: None,
            description: None,
            mention: None,
        })
        .collect();
    let capture = QueueEnqueue::new(owner(&client), manifest).expect("small metadata capture");
    assert!(capture.body().len() < 10_000);
    let records = task.await.expect("fixture");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].body.len(), QUEUE_BLOB_BYTES);
    assert_eq!(records[1].body.len(), QUEUE_BLOB_BYTES);
    assert!(records[0].body.iter().all(|byte| *byte == 0xff));
    assert!(records[1].body.iter().all(|byte| *byte == 0));
}

#[tokio::test]
async fn scripted_download_is_exact_length_bounded_and_explicitly_unverified() {
    for chunked in [false, true] {
        let (client, task) = serve(vec![Reply::binary(
            vec![0, 255, 7],
            &hex_digest(&SHA),
            chunked,
        )])
        .await;
        let mut descriptor = file("image_1").attachment;
        descriptor.size = 3;
        let handle = QueueAttachmentHandle::new(scope(&client), descriptor).expect("handle");
        let bytes = handle.download().await.expect("bounded raw bytes");
        assert_eq!(bytes.bytes(), [0, 255, 7]);
        assert_eq!(bytes.expected_sha256(), SHA);
        // SHA here is deliberately unrelated to these fixture bytes. The type
        // must not claim that matching a response header verified their digest.
        assert!(format!("{bytes:?}").contains("UnverifiedQueueBytes"));
        let requests = task.await.expect("fixture");
        assert!(requests[0].head.starts_with("GET /reverse/"));
        assert!(
            requests[0]
                .head
                .contains("/requests/original_upload_1/attachments/image_1?directory=")
        );
        assert!(requests[0].body.is_empty());
    }
    for (bytes, sha) in [
        (vec![1, 2], hex_digest(&SHA)),
        (vec![1, 2, 3, 4], hex_digest(&SHA)),
        (vec![1, 2, 3], hex_digest(&[0; 32])),
        (vec![1, 2, 3], "AB".repeat(32)),
        (vec![1, 2, 3], "bad".to_owned()),
    ] {
        let (client, task) = serve(vec![Reply::binary(bytes, &sha, true)]).await;
        let mut descriptor = file("image_1").attachment;
        descriptor.size = 3;
        assert!(
            QueueAttachmentHandle::new(scope(&client), descriptor)
                .expect("handle")
                .download()
                .await
                .is_err()
        );
        task.await.expect("fixture");
    }
    let (client, task) = serve(vec![Reply::binary(
        vec![7; QUEUE_BLOB_BYTES],
        &hex_digest(&SHA),
        true,
    )])
    .await;
    let handle =
        QueueAttachmentHandle::new(scope(&client), file("image_1").attachment).expect("handle");
    assert_eq!(
        handle
            .download()
            .await
            .expect("inclusive limit")
            .bytes()
            .len(),
        QUEUE_BLOB_BYTES
    );
    task.await.expect("fixture");
    let (client, task) = serve(vec![Reply::binary(
        vec![7; QUEUE_BLOB_BYTES + 1],
        &hex_digest(&SHA),
        true,
    )])
    .await;
    let handle =
        QueueAttachmentHandle::new(scope(&client), file("image_1").attachment).expect("handle");
    assert!(handle.download().await.is_err(), "chunked bound plus one");
    task.await.expect("fixture");
}

#[tokio::test]
async fn declared_unknown_is_distinct_from_bare_404_conflict_pending_and_network_failure() {
    for (status, code, expected) in [
        (404, None, Some(QueueManifestError::Http { status: 404 })),
        (404, Some("queue_request_unknown"), None),
        (
            409,
            Some("queue_request_conflict"),
            Some(QueueManifestError::Conflict),
        ),
        (
            409,
            Some("queue_prefix_stale"),
            Some(QueueManifestError::PrefixStale),
        ),
        (409, None, Some(QueueManifestError::Http { status: 409 })),
        (
            409,
            Some("queue_request_pending"),
            Some(QueueManifestError::Pending),
        ),
        (
            503,
            Some("queue_request_pending"),
            Some(QueueManifestError::Pending),
        ),
        (
            503,
            Some("queue_request_recovery_required"),
            Some(QueueManifestError::RecoveryRequired),
        ),
        (
            500,
            Some("queue_request_unknown"),
            Some(QueueManifestError::Http { status: 500 }),
        ),
    ] {
        let (client, task) = serve(vec![Reply::error(status, code)]).await;
        let result = owner(&client).lookup(None).await;
        match expected {
            None => assert_eq!(result, Ok(QueueLookup::Unknown)),
            Some(error) => assert_eq!(result, Err(error)),
        }
        let records = task.await.expect("fixture");
        assert_eq!(records.len(), 1);
        assert!(records[0].body.is_empty());
        let line = records[0].head.lines().next().expect("request line");
        let target = line.split_whitespace().nth(1).expect("target");
        let url = Url::parse(&format!("http://fixture{target}")).expect("request URL");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("directory".into(), DIRECTORY.into())]
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("disconnect listener");
    let address = listener.local_addr().expect("address");
    let disconnect = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("disconnect connection");
        read_request(&mut socket).await
    });
    let client = make_client(&format!("http://{address}/"));
    assert_eq!(
        owner(&client).lookup(None).await,
        Err(QueueManifestError::RequestFailed)
    );
    assert!(
        disconnect
            .await
            .expect("disconnect fixture")
            .head
            .starts_with("GET ")
    );
}

#[tokio::test]
async fn durable_operations_keep_the_same_request_body_ids_and_captured_configuration() {
    let enqueue_reply = enqueue_json();
    let take_reply = take_json(false);
    let ack_reply = take_json(true);
    let mut retry_reply = enqueue_json();
    retry_reply["attempt"] = json!(1);
    let (client, task) = serve(vec![
        Reply::json(enqueue_reply.clone()),
        Reply::json(enqueue_reply),
        Reply::json(take_reply.clone()),
        Reply::json(take_reply),
        Reply::json(ack_reply.clone()),
        Reply::json(ack_reply),
        Reply::json(retry_reply),
    ])
    .await;
    let enqueue = QueueEnqueue::new(owner(&client), item()).expect("capture");
    let accepted = enqueue.submit().await.expect("enqueue receipt");
    let replayed = enqueue
        .submit()
        .await
        .expect("explicit same capture repeat");
    assert_eq!(accepted, replayed);
    assert_eq!(accepted.receipt.message_id.as_str(), "msg_server_original");
    assert_eq!(
        accepted
            .receipt
            .context_ids
            .iter()
            .map(MessageId::as_str)
            .collect::<Vec<_>>(),
        contexts().iter().map(String::as_str).collect::<Vec<_>>()
    );
    let take =
        QueueTake::new(owner(&client), vec![QueuedItemId::new("queued_a")]).expect("take capture");
    let transferred = take.submit().await.expect("full manifest");
    assert_eq!(
        take.lookup().await.expect("lookup"),
        Some(transferred.clone())
    );
    let QueueTakeState::Transferring { items } = &transferred.state else {
        panic!("full transfer remains owned");
    };
    assert_eq!(items[0].item.send_config, config_fixture());
    assert_eq!(
        items[0].item.prompt_parts.skills[0]
            .mention
            .as_ref()
            .expect("skill mention")
            .text,
        "@skill"
    );
    let ack = QueueAcknowledge::new(owner(&client), transferred.receipt)
        .expect("explicit journal ownership acknowledgement");
    assert!(matches!(
        ack.submit().await.expect("ack").state,
        QueueTakeState::Acknowledged
    ));
    assert!(matches!(
        ack.submit().await.expect("repeat ack").state,
        QueueTakeState::Acknowledged
    ));
    let retry = QueueRetry::new(owner(&client), accepted.receipt, 1).expect("journaled retry");
    assert_eq!(retry.submit().await.expect("retry receipt").attempt, 1);
    let records = task.await.expect("fixture");
    assert_eq!(records[0].body, enqueue.body());
    assert_eq!(
        records[0].body, records[1].body,
        "explicit repeats preserve exact captured bytes"
    );
    assert_eq!(records[2].body, take.body());
    assert!(records[3].body.is_empty(), "lookup has no body");
    assert_eq!(
        serde_json::from_slice::<Value>(&records[4].body).expect("ack JSON"),
        json!({"directory":DIRECTORY})
    );
    assert_eq!(records[4].body, records[5].body);
    assert_eq!(
        serde_json::from_slice::<Value>(&records[6].body).expect("retry JSON"),
        json!({"directory":DIRECTORY,"attempt":1})
    );
    for index in [0, 1, 2, 4, 5, 6] {
        assert!(
            !records[index]
                .head
                .lines()
                .next()
                .expect("line")
                .contains('?'),
            "POST rejects query parameters"
        );
    }
}

#[tokio::test]
async fn five_20_mib_descriptors_return_a_typed_accepted_size_block_on_enqueue_and_lookup() {
    let mut input = item();
    input.attachment_manifest = (0..5)
        .map(|index| file(&format!("image_{index}")))
        .collect();
    let mut response = enqueue_json();
    response["blocked"] = json!("prompt_body_too_large");
    let (client, task) = serve(vec![
        Reply::json(response.clone()),
        Reply::json(response.clone()),
        Reply::json(response),
    ])
    .await;
    let capture = QueueEnqueue::new(owner(&client), input)
        .expect("small manifest for five large prepared files");
    assert!(capture.body().len() < 10_000);
    let accepted = capture.submit().await.expect("durable accepted size hold");
    assert_eq!(accepted.state, QueueEnqueueState::Queued);
    assert_eq!(accepted.attempt, 0);
    assert_eq!(accepted.blocked, Some(QueueBlocked::PromptBodyTooLarge));
    assert_eq!(
        capture.lookup().await.expect("exact capture lookup"),
        Some(accepted.clone())
    );
    let lookup = owner(&client)
        .lookup(Some(&QueueReceipt::Enqueue(accepted.receipt.clone())))
        .await
        .expect("retained receipt lookup");
    assert!(
        matches!(lookup, QueueLookup::Found(record) if *record == QueueRecord::Enqueue(accepted))
    );
    let records = task.await.expect("fixture");
    assert_eq!(records.len(), 3, "no delivery retry or implicit release");
    assert_eq!(records[0].body, capture.body());
    assert!(records[1..].iter().all(|record| record.body.is_empty()));
}

#[tokio::test]
async fn lookup_checks_a_retained_receipt_hash_and_never_fetches_a_growing_queue() {
    let fixture_client = make_client("https://example.invalid/");
    let expected = parse_record(
        &serde_json::to_vec(&enqueue_json()).expect("JSON"),
        &owner(&fixture_client),
    )
    .expect("record")
    .receipt();
    let mut changed = enqueue_json();
    changed["receipt"]["requestHash"] = json!(hex_digest(&[0; 32]));
    let (client, task) = serve(vec![Reply::json(enqueue_json()), Reply::json(changed)]).await;
    assert!(matches!(
        owner(&client).lookup(Some(&expected)).await,
        Ok(QueueLookup::Found(_))
    ));
    assert_eq!(
        owner(&client).lookup(Some(&expected)).await,
        Err(QueueManifestError::ReceiptMismatch)
    );
    let records = task.await.expect("fixture");
    assert_eq!(
        records.len(),
        2,
        "recovery reads only the bounded exact receipt"
    );
    assert!(records.iter().all(|request| {
        request
            .head
            .contains("/requests/request_original_1?directory=")
    }));
}

#[tokio::test]
async fn acknowledge_and_retry_refuse_receipts_with_changed_server_identities() {
    let fixture_client = make_client("https://example.invalid/");
    let QueueRecord::Take(take) = parse_record(
        &serde_json::to_vec(&take_json(false)).expect("JSON"),
        &owner(&fixture_client),
    )
    .expect("take record") else {
        panic!("take receipt");
    };
    let QueueRecord::Enqueue(enqueue) = parse_record(
        &serde_json::to_vec(&enqueue_json()).expect("JSON"),
        &owner(&fixture_client),
    )
    .expect("enqueue record") else {
        panic!("enqueue receipt");
    };
    let mut changed_take = take_json(true);
    changed_take["receipt"]["requestHash"] = json!(hex_digest(&[0; 32]));
    let mut changed_enqueue = enqueue_json();
    changed_enqueue["attempt"] = json!(1);
    changed_enqueue["receipt"]["messageId"] = json!("msg_different");
    let (client, task) = serve(vec![
        Reply::json(changed_take),
        Reply::json(changed_enqueue),
    ])
    .await;
    assert_eq!(
        QueueAcknowledge::new(owner(&client), take.receipt)
            .expect("capture")
            .submit()
            .await,
        Err(QueueManifestError::ReceiptMismatch)
    );
    assert_eq!(
        QueueRetry::new(owner(&client), enqueue.receipt, 1)
            .expect("capture")
            .submit()
            .await,
        Err(QueueManifestError::ReceiptMismatch)
    );
    assert_eq!(task.await.expect("fixture").len(), 2);
}

#[tokio::test]
async fn prefix_preview_and_reserve_preserve_hold_identity_full_items_and_authoritative_context_ids()
 {
    let fixture_client = make_client("https://example.invalid/");
    let fixture = QueuePrefixCapture::new(scope(&fixture_client), prefix()).expect("capture");
    let response = prefix_json(&fixture);
    let (client, task) = serve(vec![
        Reply::json(response.clone()),
        Reply::json(response.clone()),
        Reply::json(response),
    ])
    .await;
    let capture = QueuePrefixCapture::new(scope(&client), prefix()).expect("capture");
    let preview = capture.preview().await.expect("preview");
    let reserved = capture.reserve().await.expect("reservation");
    assert_eq!(reserved, preview);
    assert_eq!(
        capture.reserve().await.expect("lost-answer recovery"),
        reserved
    );
    assert_eq!(
        reserved
            .context_ids
            .last()
            .expect("composer context")
            .as_str(),
        "msg_composer"
    );
    assert_eq!(
        reserved.items[0].item.prompt_parts.metadata,
        item().prompt_parts.metadata
    );
    assert_eq!(reserved.items[0].item.prompt_parts.agents.len(), 2);
    assert_eq!(reserved.items[0].item.prompt_parts.skills.len(), 2);
    assert_eq!(reserved.reservation.send_config, config_fixture());
    let records = task.await.expect("fixture");
    assert!(
        records[0]
            .head
            .starts_with("POST /reverse/api/message-queue/sessions/ses_fixture/preview-prefix ")
    );
    assert!(
        records[1]
            .head
            .starts_with("POST /reverse/api/message-queue/sessions/ses_fixture/reserve-prefix ")
    );
    assert_eq!(records[0].body, records[1].body);
    assert_eq!(records[1].body, records[2].body);
    for record in &records {
        let body: Value = serde_json::from_slice(&record.body).expect("prefix JSON");
        assert_eq!(
            body["directory"], DIRECTORY,
            "host prefixes retain their exact owner in the body"
        );
        assert!(
            !record
                .head
                .to_ascii_lowercase()
                .contains("x-opencode-directory:")
        );
    }
    let (client, task) = serve(vec![Reply::error(404, None)]).await;
    let capture = QueuePrefixCapture::new(scope(&client), prefix()).expect("capture");
    assert_eq!(
        capture.reserve().await,
        Err(QueueManifestError::Http { status: 404 })
    );
    assert_eq!(
        task.await.expect("fixture").len(),
        1,
        "unsupported reservation never falls back to destructive take"
    );
}

#[tokio::test]
async fn prefix_rejects_changed_composer_selection_order_explicit_context_and_delivery() {
    let fixture_client = make_client("https://example.invalid/");
    let fixture = QueuePrefixCapture::new(scope(&fixture_client), prefix()).expect("capture");
    for field in [
        "composer",
        "id",
        "delivery",
        "itemIds",
        "sendConfig",
        "contextIds",
    ] {
        let mut value = prefix_json(&fixture);
        match field {
            "itemIds" => value["reservation"][field] = json!(["queued_other"]),
            "sendConfig" => value["reservation"][field]["modelID"] = json!("changed"),
            "contextIds" => value[field][4] = json!("msg_wrong_explicit"),
            "id" => value["reservation"][field] = json!("msg_other"),
            "delivery" => value["reservation"][field] = json!(null),
            _ => value["reservation"][field] = json!("changed"),
        }
        let (client, task) = serve(vec![Reply::json(value)]).await;
        assert!(
            QueuePrefixCapture::new(scope(&client), prefix())
                .expect("capture")
                .reserve()
                .await
                .is_err(),
            "reject {field}"
        );
        task.await.expect("fixture");
    }
}

#[test]
fn prefix_restore_requires_the_exact_captured_directory_without_normalization() {
    let client = make_client("https://example.invalid/");
    let exact_scope = QueueScope::new(
        &client,
        SessionId::new("ses_fixture"),
        "C:\\exact project\\",
    )
    .expect("exact remote owner");
    let capture = QueuePrefixCapture::new(exact_scope.clone(), prefix()).expect("capture");
    let restored = QueuePrefixCapture::from_body(exact_scope.clone(), capture.body().to_vec())
        .expect("same exact owner");
    assert_eq!(restored.body(), capture.body());
    let mut body: Value = serde_json::from_slice(capture.body()).expect("JSON");
    assert_eq!(body["directory"], "C:\\exact project\\");
    body["directory"] = json!("C:/exact project");
    assert!(
        matches!(
            QueuePrefixCapture::from_body(
                exact_scope.clone(),
                serde_json::to_vec(&body).expect("JSON")
            ),
            Err(QueueManifestError::InvalidInput)
        ),
        "lexical aliases cannot change a captured owner"
    );
    body.as_object_mut().expect("body").remove("directory");
    assert!(
        matches!(
            QueuePrefixCapture::from_body(exact_scope, serde_json::to_vec(&body).expect("JSON")),
            Err(QueueManifestError::InvalidInput)
        ),
        "an old unscoped request cannot acquire ownership"
    );
}

#[tokio::test]
async fn prefix_directory_is_sent_on_preview_reserve_and_existing_reservation_recovery() {
    let fixture_client = make_client("https://example.invalid/");
    let fixture = QueuePrefixCapture::new(scope(&fixture_client), prefix()).expect("capture");
    let mut response = prefix_json(&fixture);
    let valid_response = response.clone();
    response["reservation"]
        .as_object_mut()
        .expect("reservation")
        .remove("directory");
    let mut changed_owner = prefix_json(&fixture);
    changed_owner["reservation"]["directory"] = json!("/queue/moved-to-b");
    let (client, task) = serve(vec![
        Reply::error(409, Some("queue_request_conflict")),
        Reply::error(409, Some("queue_request_conflict")),
        Reply::json(valid_response),
        Reply::json(changed_owner.clone()),
        Reply::json(changed_owner),
        Reply::json(response),
    ])
    .await;
    let capture = QueuePrefixCapture::new(scope(&client), prefix()).expect("capture");
    assert_eq!(capture.preview().await, Err(QueueManifestError::Conflict));
    assert_eq!(capture.reserve().await, Err(QueueManifestError::Conflict));
    let recovered = capture
        .reserve()
        .await
        .expect("same-owner reservation recovery");
    assert_eq!(recovered.directory, DIRECTORY);
    assert_eq!(
        capture.preview().await,
        Err(QueueManifestError::ReceiptMismatch)
    );
    assert_eq!(
        capture.reserve().await,
        Err(QueueManifestError::ReceiptMismatch)
    );
    assert_eq!(
        capture.reserve().await,
        Err(QueueManifestError::InvalidResponse)
    );
    let records = task.await.expect("fixture");
    assert_eq!(records.len(), 6);
    for record in records {
        let body: Value = serde_json::from_slice(&record.body).expect("prefix body");
        assert_eq!(body["directory"], DIRECTORY);
        assert_eq!(body["id"], "msg_prefix_original");
        assert_eq!(body["itemIds"], json!(["queued_a"]));
    }
}

#[test]
fn captured_integer_metadata_rejects_precision_loss_in_all_owned_content_paths() {
    let client = make_client("https://example.invalid/");
    for value in [
        json!(MAX_SAFE_INTEGER + 2),
        json!(-(MAX_SAFE_INTEGER as i64) - 2),
        json!(u64::MAX),
        json!(i64::MIN),
        json!(MAX_SAFE_INTEGER + 1),
    ] {
        let metadata = Metadata(Map::from_iter([(
            "nested".to_owned(),
            json!([{"integer":value}]),
        )]));
        assert_eq!(
            validate_metadata(&metadata),
            Err(QueueManifestError::InvalidInput)
        );
        let mut input = item();
        input.prompt_parts.metadata = Some(metadata.clone());
        assert!(matches!(
            QueueEnqueue::new(owner(&client), input.clone()),
            Err(QueueManifestError::InvalidInput)
        ));
        let body = serde_json::to_vec(&json!({"directory":DIRECTORY,"item":item_value(&input)}))
            .expect("retained input JSON");
        assert!(matches!(
            QueueEnqueue::from_body(owner(&client), body),
            Err(QueueManifestError::InvalidInput)
        ));

        let mut composer = prefix();
        composer.prompt_parts.as_mut().expect("parts").metadata = Some(metadata.clone());
        assert!(matches!(
            QueuePrefixCapture::new(scope(&client), composer),
            Err(QueueManifestError::InvalidInput)
        ));
        let capture =
            QueuePrefixCapture::new(scope(&client), prefix()).expect("valid base capture");
        let mut body: Value = serde_json::from_slice(capture.body()).expect("JSON");
        body["promptParts"]["metadata"] = json!(metadata);
        assert!(matches!(
            QueuePrefixCapture::from_body(scope(&client), serde_json::to_vec(&body).expect("JSON")),
            Err(QueueManifestError::InvalidInput)
        ));

        let mut context_input = item();
        context_input.prompt_parts.context[0].metadata = Some(metadata.clone());
        assert!(matches!(
            QueueEnqueue::new(owner(&client), context_input),
            Err(QueueManifestError::InvalidInput)
        ));
        let mut legacy_input = item();
        let QueueLegacyContext::Context {
            metadata: legacy_metadata,
            ..
        } = &mut legacy_input.context[0]
        else {
            panic!("legacy context");
        };
        *legacy_metadata = metadata;
        assert!(matches!(
            QueueEnqueue::new(owner(&client), legacy_input),
            Err(QueueManifestError::InvalidInput)
        ));
    }
    for value in [
        json!(MAX_SAFE_INTEGER),
        json!(-(MAX_SAFE_INTEGER as i64)),
        json!(1.0),
        json!(-0.0),
        json!(1.0000000000000001e18_f64),
        json!(f64::MAX),
        json!(f64::MIN_POSITIVE),
    ] {
        let mut input = item();
        input.prompt_parts.metadata =
            Some(Metadata(Map::from_iter([("number".to_owned(), value)])));
        QueueEnqueue::new(owner(&client), input)
            .expect("safe integers and finite f64 values retain their Number value");
    }
}

#[tokio::test]
async fn direct_reserve_accepts_javascript_normalized_metadata_and_retained_receipts_compare_semantically()
 {
    let fixture_client = make_client("https://example.invalid/");
    let mut input = prefix();
    let parts = input.prompt_parts.as_mut().expect("parts");
    parts.metadata = Some(captured_numeric_metadata());
    parts.context[0].metadata = Some(captured_numeric_metadata());
    let fixture =
        QueuePrefixCapture::new(scope(&fixture_client), input.clone()).expect("numeric capture");
    let mut response = prefix_json(&fixture);
    assert!(
        captured_numeric_metadata().0["one"]
            .as_number()
            .expect("captured number")
            .is_f64()
    );
    assert!(
        javascript_numeric_metadata()["one"]
            .as_number()
            .expect("returned number")
            .is_u64()
    );
    let normalized = json!(javascript_numeric_metadata());
    response["reservation"]["promptParts"]["metadata"] = normalized.clone();
    response["reservation"]["promptParts"]["context"][0]["metadata"] = normalized.clone();
    response["items"][0]["promptParts"]["metadata"] = normalized.clone();
    response["items"][0]["promptParts"]["context"][0]["metadata"] = normalized.clone();
    response["items"][0]["context"][0]["metadata"] = normalized;
    let (client, task) = serve(vec![
        Reply::json(response.clone()),
        Reply::json(response.clone()),
        Reply::json(response),
    ])
    .await;
    let capture = QueuePrefixCapture::new(scope(&client), input).expect("capture");
    let reserved = capture
        .reserve()
        .await
        .expect("direct reserve accepts JavaScript Number normalization");
    assert_eq!(&reserved.reservation, capture.input());
    let preview = capture.preview().await.expect("same semantic preview");
    assert_eq!(preview, reserved);
    assert_eq!(capture.reserve().await.expect("same-ID recovery"), reserved);
    let mut retained = reserved.clone();
    let mut retained_metadata = captured_numeric_metadata();
    retained_metadata.0.insert("one".to_owned(), json!(1));
    retained_metadata
        .0
        .insert("nested".to_owned(), json!([{"n":1,"zero":0}]));
    assert_ne!(
        retained_metadata,
        captured_numeric_metadata(),
        "Serde distinguishes the retained integer representation"
    );
    retained
        .reservation
        .prompt_parts
        .as_mut()
        .expect("parts")
        .metadata = Some(retained_metadata.clone());
    retained
        .reservation
        .prompt_parts
        .as_mut()
        .expect("parts")
        .context[0]
        .metadata = Some(retained_metadata.clone());
    retained.items[0].item.prompt_parts.metadata = Some(retained_metadata.clone());
    retained.items[0].item.prompt_parts.context[0].metadata = Some(retained_metadata.clone());
    let QueueLegacyContext::Context { metadata, .. } = &mut retained.items[0].item.context[0]
    else {
        panic!("legacy context");
    };
    *metadata = retained_metadata;
    assert_eq!(
        retained, reserved,
        "retained complete receipts compare nested Number values across representations"
    );
    let mut wrong_owner = retained.clone();
    wrong_owner.directory = "/queue/moved-to-b".to_owned();
    assert_ne!(
        wrong_owner, reserved,
        "retained receipt equality preserves the exact directory owner"
    );
    let mut changed = retained.clone();
    changed
        .reservation
        .prompt_parts
        .as_mut()
        .expect("parts")
        .metadata
        .as_mut()
        .expect("metadata")
        .0
        .insert("one".to_owned(), json!(2));
    assert_ne!(
        changed, reserved,
        "numeric equality cannot hide a changed value"
    );
    let records = task.await.expect("fixture");
    assert!(records[0].head.contains("/reserve-prefix "));
    assert_eq!(
        records[0].body,
        capture.body(),
        "semantic comparison does not rewrite captured request bytes"
    );
    assert_eq!(records[0].body, records[2].body);
}

#[tokio::test]
async fn explicit_release_checks_descriptor_and_refuses_owned_blobs_without_implicit_cleanup() {
    let mut response = upload_json();
    response["state"] = json!("released");
    let descriptor =
        descriptor(serde_json::from_value(response["attachment"].clone()).expect("wire"))
            .expect("descriptor");
    let (client, task) = serve(vec![Reply::error(409, None), Reply::json(response)]).await;
    let handle = QueueAttachmentHandle::new(scope(&client), descriptor).expect("handle");
    assert_eq!(
        handle.release().await,
        Err(QueueManifestError::Http { status: 409 })
    );
    handle
        .release()
        .await
        .expect("explicit unreferenced release");
    let records = task.await.expect("fixture");
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.head.starts_with("DELETE ") && record.body.is_empty())
    );
}

#[tokio::test]
async fn every_operation_uses_the_shared_401_watch_before_feature_parsing() {
    for operation in [
        "upload",
        "download",
        "release",
        "lookup",
        "enqueue",
        "take",
        "acknowledge",
        "retry",
        "preview",
        "reserve",
    ] {
        let (client, task) = serve(vec![Reply::error(401, None)]).await;
        let watch = client.authentication_failures();
        let fixture_owner = owner(&client);
        let result = match operation {
            "upload" => QueueUpload::new(fixture_owner, "a", "x.png", "image/png", SHA, vec![1])
                .expect("upload")
                .submit()
                .await
                .map(|_| ()),
            "download" => QueueAttachmentHandle::new(scope(&client), file("image_1").attachment)
                .expect("handle")
                .download()
                .await
                .map(|_| ()),
            "release" => {
                QueueAttachmentHandle::new(scope(&client), file("image_1").attachment)
                    .expect("handle")
                    .release()
                    .await
            }
            "lookup" => fixture_owner.lookup(None).await.map(|_| ()),
            "enqueue" => QueueEnqueue::new(fixture_owner, item())
                .expect("capture")
                .submit()
                .await
                .map(|_| ()),
            "take" => QueueTake::new(fixture_owner, vec![QueuedItemId::new("queued_a")])
                .expect("capture")
                .submit()
                .await
                .map(|_| ()),
            "acknowledge" => {
                let QueueRecord::Take(record) = parse_record(
                    &serde_json::to_vec(&take_json(true)).expect("JSON"),
                    &fixture_owner,
                )
                .expect("record") else {
                    panic!("take");
                };
                QueueAcknowledge::new(fixture_owner, record.receipt)
                    .expect("capture")
                    .submit()
                    .await
                    .map(|_| ())
            }
            "retry" => {
                let QueueRecord::Enqueue(record) = parse_record(
                    &serde_json::to_vec(&enqueue_json()).expect("JSON"),
                    &fixture_owner,
                )
                .expect("record") else {
                    panic!("enqueue");
                };
                QueueRetry::new(fixture_owner, record.receipt, 1)
                    .expect("capture")
                    .submit()
                    .await
                    .map(|_| ())
            }
            "preview" => QueuePrefixCapture::new(scope(&client), prefix())
                .expect("capture")
                .preview()
                .await
                .map(|_| ()),
            _ => QueuePrefixCapture::new(scope(&client), prefix())
                .expect("capture")
                .reserve()
                .await
                .map(|_| ()),
        };
        assert_eq!(
            result,
            Err(QueueManifestError::AuthenticationRequired),
            "{operation}"
        );
        assert_eq!(
            *watch.borrow(),
            1,
            "{operation} signals the shared captured-client watch"
        );
        assert_eq!(
            task.await.expect("fixture").len(),
            1,
            "no automatic replay after a 401"
        );
    }
}

#[tokio::test]
async fn redirects_are_not_followed_and_oversized_json_never_grants_receipt_authority() {
    let mut redirect = Reply::error(307, None);
    redirect.headers.push((
        "Location".to_owned(),
        "http://example.invalid/stolen".to_owned(),
    ));
    let (client, task) = serve(vec![redirect]).await;
    assert_eq!(
        owner(&client).lookup(None).await,
        Err(QueueManifestError::Redirected)
    );
    assert_eq!(task.await.expect("fixture").len(), 1);
    for chunked in [false, true] {
        let reply = Reply {
            status: 200,
            content_type: "application/json",
            headers: vec![],
            bytes: vec![b' '; QUEUE_MANIFEST_BYTES + 1],
            chunked,
        };
        let (client, task) = serve(vec![reply]).await;
        assert_eq!(
            owner(&client).lookup(None).await,
            Err(QueueManifestError::MetadataTooLarge)
        );
        task.await.expect("fixture");
    }
}
