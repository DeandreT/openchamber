//! Headless queue, steer, starter and double-Escape tests against a scripted
//! loopback server that keeps a server-owned queue.

use gpui_kit::Window;
use openchamber_native_core::draft::Draft;
use openchamber_native_core::draft_starters::StarterRef;
use openchamber_native_core::ids::MessageId;
use openchamber_native_core::message_queue::{FollowUpBehavior, QueuedItemId};
use openchamber_native_core::session::LiveStatus;
use openchamber_native_transport::client::ServerSettings;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::queue_manifest::{
    QueueAttachmentDescriptor, QueueEnqueueState, QueueIndexedPayload, QueuePayloadDocument,
    QueuePayloadIndex, QueuePayloadItem, QueuePayloadPointer, QueuePayloadUse,
};
use serde_json::json;

use super::*;

/// What the scripted server holds and how it answers.
#[derive(Default)]
struct State {
    items: Vec<Value>,
    revision: u64,
    next_item: u64,
    /// Route suffixes (`items`, `take`, `prompt`) answered with a 500.
    fail: Vec<&'static str>,
    /// A path suffix whose answer waits until this is cleared.
    hold: Option<&'static str>,
    /// Project setup reads still to fail.
    project_failures: u32,
    project_save_failures: u32,
    /// The project's starters as `{type, name, source}`.
    project_starters: Vec<Value>,
    reservation: Option<Value>,
    drop_reservation_reply: bool,
    invalidate_after_preview: bool,
    uploads: std::collections::HashMap<String, (Value, Vec<u8>)>,
    enqueue_requests: std::collections::HashMap<String, Value>,
    captures: std::collections::HashMap<String, Value>,
    payloads: std::collections::HashMap<String, FixturePayload>,
    take_requests: std::collections::HashMap<String, Value>,
    take_bodies: std::collections::HashMap<String, Vec<u8>>,
    prefix_requests: std::collections::HashMap<String, Value>,
    prefix_bodies: std::collections::HashMap<String, Vec<u8>>,
    prefix_replies: std::collections::HashMap<String, Value>,
    project_reads: usize,
    completed_project_reads: usize,
    project_failed_reads: usize,
    project_writes: usize,
    response_status: std::collections::HashMap<&'static str, &'static str>,
    broadcasts: Vec<Value>,
    drop_enqueue_reply: bool,
    drop_retry_reply: bool,
}

struct QueueServer {
    runtime: RuntimeId,
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Request>>>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Clone)]
struct FixturePayload {
    bytes: Vec<u8>,
    pointer: QueuePayloadPointer,
    message: MessageId,
    context_ids: Vec<MessageId>,
}

fn hex_digest(bytes: &[u8]) -> String {
    crate::queue_capture::sha256(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fixture_blob_id(request: &str, attachment: &str) -> String {
    hex_digest(
        &serde_json::to_vec(&["ses_queue", "/project", request, attachment])
            .expect("fixture upload owner"),
    )
}

fn pointer_bytes(pointer: &QueuePayloadPointer, state: &State) -> Vec<u8> {
    let mut bytes = Vec::new();
    for chunk in pointer.chunks() {
        let descriptor = &chunk.attachment;
        let (stored, part) =
            &state.uploads[&format!("{}/{}", descriptor.request_id, descriptor.attachment_id)];
        assert_eq!(
            stored["blobId"],
            fixture_blob_id(&descriptor.request_id, &descriptor.attachment_id)
        );
        assert_eq!(
            stored["blobId"],
            descriptor
                .blob_id
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "the pointer retains the stored blob identity"
        );
        assert_eq!(stored["requestId"], descriptor.request_id);
        assert_eq!(stored["attachmentId"], descriptor.attachment_id);
        assert_eq!(stored["filename"], descriptor.filename);
        assert_eq!(stored["mimeType"], descriptor.mime_type);
        assert_eq!(stored["size"], descriptor.size);
        assert_eq!(part.len() as u64, descriptor.size);
        assert_eq!(crate::queue_capture::sha256(part), descriptor.sha256);
        assert_eq!(stored["sha256"], hex_digest(part));
        bytes.extend_from_slice(part);
    }
    assert_eq!(bytes.len() as u64, pointer.total_bytes());
    assert_eq!(crate::queue_capture::sha256(&bytes), pointer.sha256());
    bytes
}

fn store_fixture_capture(state: &mut State, request: &str, bytes: Vec<u8>) -> FixturePayload {
    let mut chunks = Vec::new();
    for (ordinal, part) in bytes
        .chunks(openchamber_native_transport::queue_manifest::QUEUE_BLOB_BYTES)
        .enumerate()
    {
        let attachment = format!("payload-{ordinal}");
        let blob_id = fixture_blob_id(request, &attachment);
        let digest = hex_digest(part);
        let descriptor = json!({"blobId":blob_id,"requestId":request,"attachmentId":attachment,
            "filename":format!("{attachment}.json"),"mimeType":"application/json","size":part.len(),"sha256":digest});
        state.uploads.insert(
            format!("{request}/{attachment}"),
            (descriptor.clone(), part.to_vec()),
        );
        let mut chunk = descriptor;
        chunk["ordinal"] = json!(ordinal);
        chunks.push(chunk);
    }
    let pointer = json!({"format":"payload-v1","payloadManifest":{"version":1,"encoding":"utf-8-json",
        "totalBytes":bytes.len(),"sha256":hex_digest(&bytes),"chunks":chunks}});
    let pointer = QueuePayloadPointer::from_body(
        &serde_json::to_vec(&pointer).expect("fixture pointer JSON"),
    )
    .expect("real fixture custody produces a strict pointer");
    assert_eq!(pointer_bytes(&pointer, state), bytes);
    let capture: Value = serde_json::from_slice(&bytes).expect("actual stored capture");
    let context_ids = fixture_context_ids(&capture["item"], request);
    FixturePayload {
        bytes,
        pointer,
        message: MessageId::from(format!("msg_{}", hex_digest(request.as_bytes()))),
        context_ids,
    }
}

fn fixture_context_ids(item: &Value, seed: &str) -> Vec<MessageId> {
    item["promptParts"]["context"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(ordinal, context)| {
            context["id"].as_str().map_or_else(
                || {
                    MessageId::from(format!(
                        "msg_{}",
                        hex_digest(
                            &serde_json::to_vec(&[seed, &ordinal.to_string()])
                                .expect("server-owned context identity")
                        )
                    ))
                },
                |id| MessageId::from(id.to_owned()),
            )
        })
        .collect()
}

fn fixture_index(payload: &FixturePayload, usage: QueuePayloadUse) -> QueueIndexedPayload {
    let document = QueuePayloadDocument::from_bytes(
        payload.bytes.clone(),
        "/project",
        usage,
        crate::queue_capture::sha256,
    )
    .expect("the server indexes actual strict capture bytes");
    let QueuePayloadItem::Manifest(item) = document.item() else {
        panic!("fixture transfers use manifest captures");
    };
    assert!(
        item.context.is_empty(),
        "the fixture converts legacy contexts before capture"
    );
    assert!(
        item.attachment_manifest.is_empty(),
        "these transfer controls own text/context bytes only"
    );
    let ids = payload.context_ids.clone();
    let message = &payload.message;
    // Measure this fixture's actual text/context admission bodies. Rich-file
    // transfers need their own byte-backed fixture, never invented counters.
    let mut prompt = json!({"id":message.as_str(),"text":item.text,"delivery":"queue"});
    if let Some(metadata) = &item.prompt_parts.metadata {
        prompt["metadata"] = serde_json::to_value(metadata).expect("prompt metadata");
    }
    assert!(
        item.prompt_parts.agents.is_empty()
            && item.prompt_parts.skills.is_empty()
            && item.prompt_parts.file_references.is_empty(),
        "fixture admission measurement covers its explicit parts"
    );
    let prompt_bytes = serde_json::to_vec(&prompt).expect("measured prompt").len() as u64;
    let mut admission_bytes = prompt_bytes;
    let mut structured_bytes = item.text.len() as u64;
    for (context, id) in item.prompt_parts.context.iter().zip(&ids) {
        let mut body =
            json!({"id":id.as_str(),"text":context.text,"delivery":"queue","resume":false});
        structured_bytes += context.text.len() as u64;
        if let Some(description) = &context.description {
            body["description"] = json!(description);
            structured_bytes += description.len() as u64;
        }
        if let Some(metadata) = &context.metadata {
            body["metadata"] = serde_json::to_value(metadata).expect("context metadata");
            structured_bytes += serde_json::to_vec(metadata)
                .expect("measured context metadata")
                .len() as u64;
        }
        admission_bytes = admission_bytes.max(
            serde_json::to_vec(&body)
                .expect("measured context admission")
                .len() as u64,
        );
    }
    if let Some(metadata) = &item.prompt_parts.metadata {
        structured_bytes += serde_json::to_vec(metadata)
            .expect("measured prompt metadata")
            .len() as u64;
    }
    // Normalized empty agent/skill arrays are part of logical structured size.
    structured_bytes += 4;
    let text = if item.content.is_empty() {
        &item.text
    } else {
        &item.content
    };
    let units = text.encode_utf16().take(100).collect::<Vec<_>>();
    let mut preview =
        String::from_utf16(&units).expect("these fixture previews do not split a surrogate");
    if text.encode_utf16().count() > 100 {
        preview.push_str("...");
    }
    let index = QueuePayloadIndex {
        directory: "/project".into(),
        send_config: item.send_config.clone(),
        preview,
        context_preview: item
            .context_preview
            .as_ref()
            .map(|preview| preview.display()),
        entry_count: item.prompt_parts.context.len(),
        context_ids: ids,
        attachment_blob_ids: Vec::new(),
        prompt_bytes,
        admission_bytes,
        structured_bytes,
        legacy_attachment_ids: None,
    };
    let chunks = payload
        .pointer
        .chunks()
        .iter()
        .map(|chunk| chunk.attachment.clone())
        .collect::<Vec<QueueAttachmentDescriptor>>();
    let (indexed, materialized) = crate::queue_take::tests::indexed_fixture(
        payload.bytes.clone(),
        "/project",
        chunks,
        index,
        usage,
    );
    assert_eq!(
        materialized.bytes(),
        payload.bytes,
        "index construction cannot rewrite original capture bytes"
    );
    assert_eq!(indexed.pointer(), &payload.pointer);
    indexed
}

fn fixture_index_value(index: &QueueIndexedPayload) -> Value {
    serde_json::from_slice(&index.body().expect("sealed fixture index")).expect("index JSON")
}

fn capture_other_device_item(state: &mut State, item: &Value) {
    let id = item["id"].as_str().expect("server item identity");
    assert!(
        !state.payloads.contains_key(id),
        "a new other-device enqueue has a fresh server item identity"
    );
    // Legacy text/context input came from another device. Convert it once into
    // a server-owned manifest without using its display projection as a proof.
    assert!(item["attachments"].as_array().is_none_or(Vec::is_empty));
    let mut contexts = Vec::new();
    for context in item["context"].as_array().into_iter().flatten() {
        if let Some(instructions) = context["instructions"].as_str() {
            contexts.push(json!({"text":instructions}));
        }
        let mut entry = json!({"text":context["text"]});
        if let Some(metadata) = context.get("metadata") {
            entry["metadata"] = metadata.clone();
        }
        contexts.push(entry);
    }
    let capture = json!({"directory":"/project","item":{"format":"manifest-v1","content":item["content"],
        "text":item["text"],"attachmentManifest":[],"sendConfig":item["sendConfig"],
        "promptParts":{"agents":[],"skills":[],"context":contexts}}});
    let payload = store_fixture_capture(
        state,
        &format!("fixture_{}", hex_digest(id.as_bytes())),
        serde_json::to_vec(&capture).expect("original other-device capture"),
    );
    state.payloads.insert(id.to_owned(), payload);
}

fn fixture_transfer_item(state: &State, item: &Value) -> Value {
    let payload = state.payloads.get(item["id"].as_str().expect("server item ID"))
        .expect("display projection alone cannot authorize a transfer; original byte custody is required").clone();
    let indexed = fixture_index(&payload, QueuePayloadUse::Enqueue);
    let mut transferred = fixture_index_value(&indexed);
    transferred["id"] = item["id"].clone();
    transferred["createdAt"] = item["createdAt"].clone();
    transferred
}

fn session_body(state: &State) -> Value {
    // The server lists each item with its durable enqueue (`sessionSnapshot`).
    let items: Vec<Value> = state
        .items
        .iter()
        .map(|item| {
            let mut item = item.clone();
            let record = state
                .enqueue_requests
                .values()
                .find(|record| record["receipt"]["itemId"] == item["id"]);
            if let Some(record) = record {
                item["queueDelivery"] = json!({"requestId": record["receipt"]["requestId"],
                    "state": record["state"], "attempt": record["attempt"]});
            }
            item
        })
        .collect();
    json!({"sessionId": "ses_queue", "directory": "/project", "items": items, "sendingId": null})
}

fn answer(
    request: &Request,
    headers: &std::collections::HashMap<String, String>,
    state: &mut State,
) -> (&'static str, String) {
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let last = path.rsplit('/').next().unwrap_or_default().to_owned();
    if state.fail.iter().any(|fail| *fail == last) {
        return ("500 Internal Server Error", "{}".to_owned());
    }
    if let Some(tail) = path.strip_prefix("/api/message-queue/sessions/ses_queue/requests/") {
        let parts: Vec<_> = tail.split('/').collect();
        let id = parts[0];
        if request.method == "PUT" && parts.get(1) == Some(&"attachments") {
            let attachment = parts[2];
            let target = Endpoint::parse("http://fixture.invalid/")
                .expect("fixture URL parser")
                .url(&request.path);
            let query = target.query_pairs().collect::<Vec<_>>();
            assert_eq!(query.len(), 1, "uploads retain their exact directory query");
            assert_eq!(query[0].0, "directory");
            assert_eq!(query[0].1, "/project");
            let mut filename_url = target.clone();
            filename_url.set_query(Some(&format!(
                "filename={}",
                headers["x-openchamber-filename"]
            )));
            let filename = filename_url
                .query_pairs()
                .next()
                .expect("encoded upload filename")
                .1
                .into_owned();
            assert_eq!(
                openchamber_native_transport::client::encode_directory(&filename),
                headers["x-openchamber-filename"],
                "the upload filename is canonical encoded metadata"
            );
            assert!(
                openchamber_native_transport::queue_manifest::QueueUpload::is_valid_filename(
                    &filename
                ),
                "upload names remain safe basenames"
            );
            assert_eq!(headers["content-type"], "application/octet-stream");
            let length = request.body.len().to_string();
            assert_eq!(headers.get("content-length"), Some(&length));
            assert_eq!(headers.get("x-openchamber-size"), Some(&length));
            let mime_type = &headers["x-openchamber-mime-type"];
            assert!(
                mime_type
                    .split_once('/')
                    .is_some_and(|(kind, subtype)| !kind.is_empty() && !subtype.is_empty()),
                "uploads declare their MIME type"
            );
            if attachment.starts_with("payload-") {
                assert_eq!(mime_type, "application/json");
                assert_eq!(filename, format!("{attachment}.json"));
            }
            let digest = crate::queue_capture::sha256(&request.body)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(
                headers.get("x-openchamber-sha256"),
                Some(&digest),
                "uploads carry the captured raw-byte digest"
            );
            let owner_bytes =
                serde_json::to_vec(&["ses_queue", query[0].1.as_ref(), id, attachment])
                    .expect("backend blob identity preimage");
            let descriptor = json!({"blobId": crate::queue_capture::sha256(&owner_bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                "requestId": id, "attachmentId": attachment, "filename": filename,
                "mimeType": mime_type, "size": request.body.len(), "sha256": digest});
            let key = format!("{id}/{attachment}");
            if let Some((original, bytes)) = state.uploads.get(&key) {
                assert_eq!(
                    original, &descriptor,
                    "an upload label retains all original metadata"
                );
                assert_eq!(bytes, &request.body, "an upload label cannot change bytes");
            }
            state
                .uploads
                .insert(key, (descriptor.clone(), request.body.clone()));
            return (
                "200 OK",
                json!({"attachment":descriptor,"state":"available"}).to_string(),
            );
        }
        if request.method == "GET" && parts.len() == 1 {
            return state
                .enqueue_requests
                .get(id)
                .or_else(|| state.take_requests.get(id))
                .map_or_else(
                    || {
                        (
                            "404 Not Found",
                            json!({"code":"queue_request_unknown"}).to_string(),
                        )
                    },
                    |record| ("200 OK", record.to_string()),
                );
        }
        if request.method == "POST" && parts.get(1) == Some(&"take") {
            if let Some(record) = state.take_requests.get(id) {
                assert_eq!(
                    state.take_bodies[id], request.body,
                    "a Take retry retains its original request bytes"
                );
                return ("200 OK", record.to_string());
            }
            assert_eq!(body["format"], "payload-v1");
            assert_eq!(body["directory"], "/project");
            let operation = request
                .journal
                .as_ref()
                .and_then(|snapshot| snapshot["queue"]["operations"].as_array())
                .and_then(|operations| operations.iter().find(|operation| operation["id"] == id))
                .expect("a real Take owner is durable before POST");
            assert_eq!(operation["action"]["take"]["phase"], "unknown");
            assert_eq!(operation["owner"]["session"], "ses_queue");
            assert_eq!(operation["owner"]["directory"], "/project");
            assert_eq!(
                operation["action"]["take"]["request"]
                    .as_str()
                    .expect("retained Take JSON")
                    .as_bytes(),
                request.body
            );
            let ids = body["itemIds"]
                .as_array()
                .expect("explicit ordered Take IDs");
            let selected = ids
                .iter()
                .map(|id| state.items.iter().find(|item| item["id"] == *id).cloned())
                .collect::<Option<Vec<_>>>();
            let Some(selected) = selected else {
                return ("409 Conflict", "{}".into());
            };
            let items = selected
                .iter()
                .map(|item| fixture_transfer_item(state, item))
                .collect::<Vec<_>>();
            state.items.retain(|item| !ids.contains(&item["id"]));
            for original in state.enqueue_requests.values_mut() {
                if ids.contains(&original["receipt"]["itemId"]) {
                    original["state"] = json!("taken");
                }
            }
            state.revision += 1;
            let record = json!({"receipt":{"version":1,"kind":"take","requestId":id,"sessionId":"ses_queue",
                "directory":"/project","requestHash":hex_digest(&request.body),"acceptedAt":1,"itemIds":ids},
                "state":"transferring","revision":state.revision,"items":items});
            state
                .take_bodies
                .insert(id.to_owned(), request.body.clone());
            state.take_requests.insert(id.to_owned(), record.clone());
            return ("200 OK", record.to_string());
        }
        if request.method == "POST" && parts.get(1) == Some(&"acknowledge") {
            assert_eq!(body, json!({"directory":"/project"}));
            let operation = request
                .journal
                .as_ref()
                .and_then(|snapshot| snapshot["queue"]["operations"].as_array())
                .and_then(|operations| operations.iter().find(|operation| operation["id"] == id))
                .expect("acknowledgement retains its real durable Take owner");
            assert!(
                operation["action"]["take"]["phase"]["acknowledgement_unknown"].is_object(),
                "only the real local destination and acknowledgement barrier authorize this POST"
            );
            let record = state
                .take_requests
                .get_mut(id)
                .expect("only a real transferred Take can acknowledge");
            record["state"] = json!("acknowledged");
            record.as_object_mut().expect("Take record").remove("items");
            return ("200 OK", record.to_string());
        }
        if request.method == "POST" && parts.get(1) == Some(&"enqueue") {
            if let Some(record) = state.enqueue_requests.get(id) {
                return ("200 OK", record.to_string());
            }
            assert_eq!(body["item"]["format"], "payload-v1");
            let pointer = QueuePayloadPointer::from_body(
                &serde_json::to_vec(&body["item"]).expect("actual enqueue pointer"),
            )
            .expect("strict actual enqueue pointer");
            let bytes = pointer_bytes(&pointer, state);
            let capture: Value =
                serde_json::from_slice(&bytes).expect("complete structured payload");
            assert_eq!(capture["directory"], "/project");
            state.next_item += 1;
            state.revision += 1;
            let mut item = capture["item"].clone();
            item["id"] = json!(format!("q{}", state.next_item));
            item["createdAt"] = json!(state.next_item);
            // Projection checks retain expanded display data. Transfers use
            // exclusively the original bytes below, never this display row.
            item["attachments"] = json!([]);
            let contexts = fixture_context_ids(&capture["item"], id);
            let record = json!({"receipt":{"version":1,"kind":"enqueue","requestId":id,"sessionId":"ses_queue",
                "directory":"/project","requestHash":hex_digest(&request.body),"acceptedAt":1,"itemId":item["id"],
                "messageId":format!("msg_delivery_{}",state.next_item),
                "contextIds":contexts.iter().map(MessageId::as_str).collect::<Vec<_>>()},
                "state":"queued","revision":state.revision,"attempt":0});
            state.payloads.insert(
                item["id"].as_str().expect("accepted item").to_owned(),
                FixturePayload {
                    bytes,
                    pointer,
                    message: MessageId::from(format!("msg_delivery_{}", state.next_item)),
                    context_ids: contexts,
                },
            );
            state.items.push(item);
            state.captures.insert(id.into(), capture);
            state.enqueue_requests.insert(id.into(), record.clone());
            return ("200 OK", record.to_string());
        }
        if request.method == "POST" && parts.get(1) == Some(&"retry") {
            let record = state
                .enqueue_requests
                .get_mut(id)
                .expect("accepted original enqueue");
            let attempt = body["attempt"].as_u64().expect("explicit retry attempt");
            let current = record["attempt"].as_u64().expect("current attempt");
            assert!(
                attempt == current || attempt == current + 1,
                "retry cannot skip or replace an authorization"
            );
            record["attempt"] = json!(attempt);
            record["state"] = json!("queued");
            return ("200 OK", record.to_string());
        }
    }
    let changed = |state: &mut State| {
        state.revision += 1;
        json!({"revision": state.revision, "session": session_body(state)})
    };
    if path == "/api/magic-prompts" {
        return ("200 OK", r#"{"version":1,"overrides":{}}"#.to_owned());
    }
    if path.starts_with("/api/projects/") && path.ends_with("/config") {
        if request.method == "GET" {
            state.project_reads += 1;
        }
        if request.method == "GET" && state.project_failures > 0 {
            state.project_failures -= 1;
            state.project_failed_reads += 1;
            return ("500 Internal Server Error", "{}".to_owned());
        }
        if request.method == "PUT" {
            state.project_writes += 1;
            if state.project_save_failures > 0 {
                state.project_save_failures -= 1;
                return ("500 Internal Server Error", "{}".to_owned());
            }
            let personal: Vec<Value> = body["draftStarters"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut entry| {
                    entry["source"] = json!("personal");
                    entry
                })
                .collect();
            state
                .project_starters
                .retain(|entry| entry["source"] == "shared");
            state.project_starters.extend(personal);
        }
        return (
            "200 OK",
            json!({"draftStarters": state.project_starters}).to_string(),
        );
    }
    let queue = path.strip_prefix("/api/message-queue/sessions/ses_queue");
    match (request.method.as_str(), queue) {
        ("POST", Some("/reserve-prefix" | "/preview-prefix")) => {
            let preview = queue == Some("/preview-prefix");
            assert_eq!(body["format"], "payload-v1");
            assert_eq!(body["directory"], "/project");
            let message = body["id"]
                .as_str()
                .expect("captured prefix prompt ID")
                .to_owned();
            let operation = request
                .journal
                .as_ref()
                .and_then(|snapshot| snapshot["queue"]["operations"].as_array())
                .and_then(|operations| {
                    operations
                        .iter()
                        .find(|operation| operation["action"]["prefix"]["message"] == message)
                })
                .expect("the actual captured prefix identity precedes its HTTP request");
            assert_eq!(
                operation["action"]["prefix"]["request"]
                    .as_str()
                    .expect("retained Prefix JSON")
                    .as_bytes(),
                request.body
            );
            if !preview {
                assert!(
                    operation["action"]["prefix"]["phase"]["reservation_unknown"].is_object(),
                    "reserve authority is durably uncertain before POST"
                );
            }
            if let Some(original) = state.prefix_requests.get(&message) {
                assert_eq!(
                    original, &body,
                    "preview and reservation retain the same captured pointer and IDs"
                );
                assert_eq!(
                    state.prefix_bodies[&message], request.body,
                    "preview, reserve and explicit Retry retain exact request bytes"
                );
            }
            if state.reservation.is_none() {
                let ids = body["itemIds"].as_array().expect("prefix ids");
                if ids.is_empty()
                    || !ids.iter().enumerate().all(|(index, id)| {
                        state.items.get(index).is_some_and(|item| &item["id"] == id)
                    })
                {
                    // As the server: a changed queue without a reservation is stale.
                    return (
                        "409 Conflict",
                        json!({"code":"queue_prefix_stale"}).to_string(),
                    );
                }
                if !preview {
                    state.reservation = Some(body.clone());
                    state.revision += 1;
                }
            }
            let receipt = state.reservation.as_ref().unwrap_or(&body).clone();
            if receipt != body {
                return ("409 Conflict", "{}".to_owned());
            }
            let count = receipt["itemIds"].as_array().expect("ids").len();
            let mut response = if let Some(original) = state.prefix_replies.get(&message) {
                original.clone()
            } else {
                let pointer = QueuePayloadPointer::from_body(
                    &serde_json::to_vec(&body["composer"]).expect("actual composer pointer"),
                )
                .expect("strict captured composer pointer");
                let bytes = pointer_bytes(&pointer, state);
                let raw: Value =
                    serde_json::from_slice(&bytes).expect("actual uploaded composer capture");
                let composer = FixturePayload {
                    context_ids: fixture_context_ids(&raw["item"], &message),
                    bytes,
                    pointer,
                    message: MessageId::from(message.clone()),
                };
                let composer = fixture_index(&composer, QueuePayloadUse::Composer);
                let originals = state.items[..count].to_vec();
                let items = originals
                    .iter()
                    .map(|item| fixture_transfer_item(state, item))
                    .collect::<Vec<_>>();
                let mut context_ids = items
                    .iter()
                    .flat_map(|item| {
                        item["payloadIndex"]["contextIds"]
                            .as_array()
                            .expect("frozen queued contexts")
                            .iter()
                            .cloned()
                    })
                    .collect::<Vec<_>>();
                context_ids.extend(
                    composer
                        .index()
                        .context_ids
                        .iter()
                        .map(|id| json!(id.as_str())),
                );
                let mut reservation = receipt;
                reservation["composer"] = fixture_index_value(&composer);
                let response = json!({"revision":state.revision,"reservation":reservation,"items":items,"contextIds":context_ids});
                state.prefix_requests.insert(message.clone(), body.clone());
                state
                    .prefix_bodies
                    .insert(message.clone(), request.body.clone());
                state.prefix_replies.insert(message, response.clone());
                response
            };
            response["revision"] = json!(state.revision);
            if preview && std::mem::take(&mut state.invalidate_after_preview) {
                state.items.clear();
                state.revision += 1;
            }
            ("200 OK", response.to_string())
        }
        ("POST", Some("/items")) => {
            state.next_item += 1;
            let mut item = body["item"].clone();
            item["id"] = json!(format!("q{}", state.next_item));
            item["createdAt"] = json!(state.next_item);
            capture_other_device_item(state, &item);
            state.items.push(item);
            ("200 OK", changed(state).to_string())
        }
        ("POST", Some("/take")) => {
            let items = std::mem::take(&mut state.items);
            let mut response = changed(state);
            response["items"] = json!(items);
            ("200 OK", response.to_string())
        }
        ("POST", Some(rest)) if rest.ends_with("/take") => {
            let id = rest.trim_start_matches("/items/").trim_end_matches("/take");
            let Some(index) = state.items.iter().position(|item| item["id"] == id) else {
                return ("404 Not Found", "{}".to_owned());
            };
            let item = state.items.remove(index);
            let mut response = changed(state);
            response["item"] = item;
            ("200 OK", response.to_string())
        }
        ("DELETE", Some(rest)) => {
            let id = rest.trim_start_matches("/items/");
            state.items.retain(|item| item["id"] != id);
            // The server settles the item's durable enqueue with it.
            for record in state.enqueue_requests.values_mut() {
                if record["receipt"]["itemId"] == id {
                    record["state"] = json!("removed");
                }
            }
            ("200 OK", changed(state).to_string())
        }
        ("PUT", Some("/order")) => {
            let order: Vec<String> =
                serde_json::from_value(body["itemIds"].clone()).unwrap_or_default();
            state
                .items
                .sort_by_key(|item| order.iter().position(|id| item["id"] == id.as_str()));
            ("200 OK", changed(state).to_string())
        }
        ("GET", _) if path == "/api/message-queue" => (
            "200 OK",
            json!({"revision": state.revision, "sessions": [session_body(state)]}).to_string(),
        ),
        ("PUT", _) if path == "/api/config/settings" => ("200 OK", body.to_string()),
        ("GET", _) if path.ends_with("/inbox") => ("200 OK", r#"{"data":[]}"#.to_owned()),
        ("POST", _) if path.ends_with("/prompt") || path.ends_with("/synthetic") => {
            if path.ends_with("/prompt")
                && state
                    .reservation
                    .as_ref()
                    .is_some_and(|receipt| receipt["id"] == body["id"])
            {
                let receipt = state.reservation.take().expect("receipt");
                let count = receipt["itemIds"].as_array().expect("ids").len();
                state.items.drain(..count);
                state.revision += 1;
            }
            let kind = if path.ends_with("/prompt") {
                "user"
            } else {
                "synthetic"
            };
            (
                "200 OK",
                json!({"data": {"id": body["id"], "sessionID": "ses_queue", "type": kind,
                    "time": {"created": 1}, "delivery": body["delivery"].as_str().unwrap_or("queue"),
                    "payload": {}}})
                .to_string(),
            )
        }
        ("POST", _) if path.ends_with("/interrupt") => {
            ("200 OK", r#"{"interrupted":true}"#.to_owned())
        }
        ("POST", _) => ("204 No Content", String::new()),
        _ => ("404 Not Found", "{}".to_owned()),
    }
}

impl QueueServer {
    fn new(journal: PathBuf, runtime: RuntimeId) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, scripted, stopped) = (requests.clone(), state.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("read timeout");
                let (mut request, headers) = read_queue_request(&mut stream);
                request.journal = std::fs::read(&journal)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                let path = request
                    .path
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let (status, body, raw_digest) = {
                    let mut state = scripted.lock().expect("state");
                    let previous = state.revision;
                    let raw = path
                        .strip_prefix("/api/message-queue/sessions/ses_queue/requests/")
                        .and_then(|tail| tail.split_once("/attachments/"))
                        .filter(|_| request.method == "GET")
                        .and_then(|(request, attachment)| {
                            state.uploads.get(&format!("{request}/{attachment}"))
                        })
                        .map(|(descriptor, bytes)| {
                            (
                                bytes.clone(),
                                descriptor["sha256"]
                                    .as_str()
                                    .expect("owned digest")
                                    .to_owned(),
                            )
                        });
                    let response = if let Some((bytes, digest)) = raw {
                        ("200 OK", bytes, Some(digest))
                    } else {
                        let (status, body) = answer(&request, &headers, &mut state);
                        (status, body.into_bytes(), None)
                    };
                    let kind = [
                        "preview-prefix",
                        "reserve-prefix",
                        "take",
                        "acknowledge",
                        "enqueue",
                        "prompt",
                        "config",
                    ]
                    .into_iter()
                    .find(|suffix| path.ends_with(*suffix))
                    .unwrap_or("other");
                    state.response_status.insert(kind, response.0);
                    if state.revision != previous {
                        let event =
                            json!({"revision":state.revision,"session":session_body(&state)});
                        state.broadcasts.push(event);
                    }
                    response
                };
                let project_read = request.method == "GET"
                    && path.starts_with("/api/projects/")
                    && path.ends_with("/config");
                recorded.lock().expect("requests").push(request);
                if path.ends_with("/reserve-prefix") {
                    let mut state = scripted.lock().expect("state");
                    if std::mem::take(&mut state.drop_reservation_reply) {
                        continue;
                    }
                }
                if path.ends_with("/enqueue")
                    && std::mem::take(&mut scripted.lock().expect("state").drop_enqueue_reply)
                {
                    continue;
                }
                if path.ends_with("/retry")
                    && std::mem::take(&mut scripted.lock().expect("state").drop_retry_reply)
                {
                    continue;
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while scripted
                    .lock()
                    .expect("state")
                    .hold
                    .is_some_and(|suffix| path.ends_with(suffix))
                    && !stopped.load(Ordering::Acquire)
                {
                    assert!(
                        Instant::now() < deadline,
                        "the test releases the held answer"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                let content_type = if raw_digest.is_some() {
                    "application/octet-stream"
                } else {
                    "application/json"
                };
                let digest_header = raw_digest.map_or_else(String::new, |digest| {
                    format!("x-openchamber-sha256: {digest}\r\n")
                });
                if write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{digest_header}Connection: close\r\n\r\n", body.len()).is_ok()
                    && stream.write_all(&body).is_ok() && project_read {
                        scripted.lock().expect("state").completed_project_reads += 1;
                    }
            }
        });
        Self {
            runtime,
            endpoint,
            requests,
            state,
            stop,
            worker: Some(worker),
        }
    }

    fn server(&self) -> Server {
        Server {
            endpoint: self.endpoint.clone(),
            requests: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(true)),
            release_prompt: Arc::new(AtomicBool::new(true)),
            worker: None,
        }
    }

    /// `METHOD path` of every request, in order.
    fn calls(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }

    fn body_of(&self, suffix: &str) -> Value {
        let requests = self.requests.lock().expect("requests");
        let request = requests
            .iter()
            .rfind(|request| request.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("a request to {suffix}"));
        serde_json::from_slice(&request.body).expect("JSON body")
    }
}

fn read_queue_request(
    stream: &mut TcpStream,
) -> (Request, std::collections::HashMap<String, String>) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).expect("request headers");
        bytes.push(byte[0]);
        assert!(bytes.len() <= 65536, "bounded fixture headers");
    }
    let header = String::from_utf8(bytes).expect("ASCII HTTP headers");
    let mut first = header
        .lines()
        .next()
        .expect("request line")
        .split_whitespace();
    let method = first.next().expect("method").to_owned();
    let path = first.next().expect("path").to_owned();
    let headers: std::collections::HashMap<String, String> = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |length| length.parse().expect("length"));
    assert!(
        length <= 20 * 1024 * 1024,
        "fixture raw upload follows the existing chunk limit"
    );
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("complete request body");
    (
        Request {
            method,
            path,
            body,
            journal: None,
        },
        headers,
    )
}

impl Drop for QueueServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("scripted server completes");
            }
        }
    }
}

struct Harness {
    server: QueueServer,
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    key: SessionKey,
    _fixture: Fixture,
}

fn wait_queue(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    server: &QueueServer,
    label: &'static str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let mut waited = Duration::ZERO;
    loop {
        let broadcasts = std::mem::take(
            &mut server
                .state
                .lock()
                .expect("scripted live events")
                .broadcasts,
        );
        if !broadcasts.is_empty() {
            cx.update_window(window, |_, window, cx| workspace.update(cx, |workspace, cx| {
                if workspace.runtime != server.runtime { return; }
                for properties in broadcasts {
                    let frame = json!({"type":"event","payload":{"type":"openchamber:message-queue.updated","properties":properties}});
                    let events = openchamber_native_transport::event::decode_frame(&frame.to_string()).expect("actual scripted queue broadcast").events;
                    workspace.apply_server_events(events, window, cx);
                }
            })).expect("the owned window receives its server's live events");
        }
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        if waited >= Duration::from_secs(5) {
            let (ready, busy, phases, checks) = cx.read(|cx| {
                let workspace = workspace.read(cx);
                let phases = workspace
                    .queue_storage
                    .operations()
                    .iter()
                    .map(|operation| {
                        let value =
                            serde_json::to_value(operation).expect("content-free phase inspection");
                        let (kind, action) = value["action"]
                            .as_object()
                            .expect("typed action")
                            .iter()
                            .next()
                            .expect("one action");
                        let phase = &action["phase"];
                        let phase = phase
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| {
                                phase
                                    .as_object()
                                    .and_then(|phase| phase.keys().next().cloned())
                            })
                            .unwrap_or_else(|| "invalid".into());
                        format!("{kind}/{phase}")
                    })
                    .collect::<Vec<_>>();
                let checks = workspace
                    .transfer_recovery_items()
                    .iter()
                    .map(|row| row.status)
                    .collect::<Vec<_>>();
                (
                    workspace.persistence_ready(),
                    workspace.queue.any_payload_busy(),
                    phases,
                    checks,
                )
            });
            let state = server.state.lock().expect("fixture diagnostics");
            panic!(
                "{label}: ready={ready}, actors_busy={busy}, phases={phases:?}, checks={checks:?}, responses={:?}, queued={}, reservations={}, take_receipts={}, project_reads={}, failed_project_reads={}, project_writes={}",
                state.response_status,
                state.items.len(),
                usize::from(state.reservation.is_some()),
                state.take_requests.len(),
                state.project_reads,
                state.project_failed_reads,
                state.project_writes
            );
        }
        let started = Instant::now();
        thread::sleep(Duration::from_millis(1));
        waited += started.elapsed();
    }
}

fn configure_queue_session(
    workspace: &mut Workspace,
    key: &SessionKey,
    window: &mut Window,
    cx: &mut gpui_kit::Context<Workspace>,
) {
    use openchamber_native_core::draft::{DraftKey, DraftTarget};

    // Runtime switching clears topology. Install the same registered project
    // used by the starter fixtures before normal session admission runs.
    workspace.topology.projects = vec![openchamber_native_core::ownership::ConfiguredProject {
        id: openchamber_native_core::ids::ProjectId::from("registered"),
        path: "/project".into(),
    }];
    configure_send(workspace, key, window, cx);
    assert!(workspace.session_admitted(&key.session));
    assert_eq!(workspace.runtime, key.runtime);
    let session = workspace
        .selected_session()
        .expect("the fixture session is admitted and selected");
    assert_eq!(session.id, key.session);
    assert_eq!(session.directory.as_deref(), Some("/project"));
    let owner = DraftKey::new(
        key.runtime.clone(),
        "/project",
        DraftTarget::Session(key.session.clone()),
    )
    .expect("the fixture session has a valid directory");
    assert_eq!(workspace.composer_key.as_ref(), Some(&owner));
    let projected = workspace
        .selected_queue_key()
        .expect("the selected session owns its queue");
    assert_eq!(projected.runtime, key.runtime);
    assert_eq!(projected.directory, owner.directory());
    assert_eq!(projected.session, key.session);
    // Direct catalog injection needs the reconciliation normally run by render.
    workspace.refresh_composer_choices(cx);
    let (directory, selection) = workspace
        .send_selection()
        .expect("authoritative fixture selection");
    assert_eq!(directory, "/project");
    assert_eq!(selection.agent_name.as_deref(), Some("agent"));
    assert_eq!(
        selection.model,
        Some(ModelChoice {
            provider_id: "provider".into(),
            model_id: "model".into(),
            variant: Some("effort".into()),
        })
    );
}

fn harness(cx: &mut TestAppContext, follow_up: FollowUpBehavior) -> Harness {
    let fixture = Fixture::new();
    let key = tests::key("queue-runtime", "ses_queue");
    let server = QueueServer::new(fixture.journal(), key.runtime.clone());
    let (window, workspace) = open(cx, &server.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_queue_session(workspace, &key, window, cx);
            workspace.send_catalog.settings = Some(ServerSettings {
                follow_up_behavior: Some(follow_up),
                ..ServerSettings::default()
            });
            workspace
                .live_status
                .insert(key.session.clone(), LiveStatus::Busy);
        })
    })
    .expect("window");
    Harness {
        server,
        window,
        workspace,
        key,
        _fixture: fixture,
    }
}

impl Harness {
    fn wait(
        &self,
        cx: &mut TestAppContext,
        label: &'static str,
        done: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        wait_queue(cx, self.window, &self.workspace, &self.server, label, done);
    }

    fn object_bytes(
        &self,
        cx: &mut TestAppContext,
        id: &crate::queue_capture::QueueObjectId,
    ) -> Vec<u8> {
        let object = cx
            .read(|cx| self.workspace.read(cx).queue_storage.object(id).cloned())
            .expect("the actual captured object remains rooted");
        assert!(object.verified());
        let spool = self
            ._fixture
            .journal()
            .parent()
            .expect("profile")
            .join("attachment-spool");
        let mut bytes = Vec::new();
        for chunk in object.reference().chunks() {
            let descriptor = chunk.descriptor().expect("committed object descriptor");
            let part = fs::read(spool.join(format!("blob-{}.bin", descriptor.id().as_str())))
                .expect("actual rooted object bytes");
            assert_eq!(part.len() as u64, descriptor.byte_len());
            assert_eq!(crate::queue_capture::sha256(&part), *descriptor.sha256());
            bytes.extend_from_slice(&part);
        }
        assert_eq!(bytes.len() as u64, object.reference().total_bytes());
        assert_eq!(
            &crate::queue_capture::sha256(&bytes),
            object.reference().sha256()
        );
        bytes
    }

    fn act(
        &self,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut Workspace, &mut Window, &mut gpui_kit::Context<Workspace>),
    ) {
        cx.update_window(self.window, |_, window, cx| {
            self.workspace
                .update(cx, |workspace, cx| action(workspace, window, cx))
        })
        .expect("window");
    }

    fn type_and_enter(&self, cx: &mut TestAppContext, text: &str, secondary: bool) {
        self.act(cx, |workspace, window, cx| {
            workspace
                .composer
                .update(cx, |composer, cx| composer.set_value(text, window, cx));
            workspace.submit_from_enter(secondary, window, cx);
        });
    }

    fn composer(&self, cx: &mut TestAppContext) -> String {
        cx.read(|cx| {
            self.workspace
                .read(cx)
                .composer
                .read(cx)
                .value()
                .to_string()
        })
    }

    fn queued(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.read(|cx| {
            self.workspace
                .read(cx)
                .selected_queue()
                .iter()
                .map(|message| message.content.clone())
                .collect()
        })
    }

    /// Rereads the host's queue, as a queue broadcast would deliver it.
    fn reread(&self, cx: &mut TestAppContext) {
        self.act(cx, |workspace, _, cx| workspace.read_queue_owner(None, cx));
    }

    /// Rereads the queue and waits for the recovery row that listing gives
    /// the enqueue `id`.
    fn reread_until_row(
        &self,
        cx: &mut TestAppContext,
        id: &crate::queue_capture::QueueOperationId,
    ) {
        self.reread(cx);
        wait(cx, |cx| {
            cx.read(|cx| {
                self.workspace
                    .read(cx)
                    .enqueue_recovery_items()
                    .iter()
                    .any(|row| &row.id == id)
            })
        });
    }

    /// Draws one frame, which is where a queued message's departure is
    /// picked up. Without `rows`, the frame must hold no enqueue recovery row.
    fn frame(&self, cx: &mut TestAppContext, rows: bool) {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                rows || self.workspace.read(cx).enqueue_recovery_items().is_empty(),
                "no enqueue recovery row is drawn in this frame"
            );
        })
        .expect("window");
    }

    /// Draws a frame after every task the executor can step, until `done`.
    fn frames_until(
        &self,
        cx: &mut TestAppContext,
        rows: bool,
        mut done: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        let mut waited = Duration::ZERO;
        loop {
            self.frame(cx, rows);
            let mut steps = 0;
            while steps < 256 && cx.executor().tick() {
                self.frame(cx, rows);
                steps += 1;
            }
            cx.run_until_parked();
            self.frame(cx, rows);
            if done(cx) {
                return;
            }
            assert!(
                waited < Duration::from_secs(5),
                "background work completes within five seconds of drawing"
            );
            let started = Instant::now();
            thread::sleep(Duration::from_millis(1));
            waited += started.elapsed();
        }
    }

    fn lookups(&self) -> usize {
        count(
            &self.server,
            "GET /api/message-queue/sessions/ses_queue/requests/",
        )
    }

    fn queued_in(&self, cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<String> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .selected_queue()
                .iter()
                .map(|message| message.content.clone())
                .collect()
        })
    }

    fn idle(&self, cx: &mut TestAppContext) {
        self.wait(cx, "all captured send actors settle", |cx| {
            cx.read(|cx| {
                let workspace = self.workspace.read(cx);
                !workspace.send_attempts.is_active(&self.key) && !workspace.queue.any_payload_busy()
            })
        });
    }
}

#[gpui_kit::test]
fn a_picker_completed_during_a_close_barrier_can_be_opened_after_close_fails(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        workspace.choose_attachment_paths(window, cx)
    });
    assert!(cx.did_prompt_for_paths());
    harness.act(cx, |workspace, _, _| {
        workspace.attachment_storage_close = Some(gpui_kit::Task::ready(()));
    });
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && options.multiple && !options.directories);
        None
    });
    cx.run_until_parked();
    harness.act(cx, |workspace, window, cx| {
        workspace.attachment_storage_close = None;
        workspace.choose_attachment_paths(window, cx);
    });
    assert!(
        cx.did_prompt_for_paths(),
        "a completed dialog must not occupy picker ownership after a failed close"
    );
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn legacy_queue_routes_leave_files_and_context_with_their_original_text(cx: &mut TestAppContext) {
    use openchamber_native_core::draft_content::{DraftContext, DraftFile, DraftFileContent};
    use openchamber_native_core::ids::{AttachmentId, ContextItemId};

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value("complete follow up", window, cx)
        });
        workspace.save_composer_draft(cx);
        let owner = workspace.composer_key.clone().expect("owner");
        let mut draft = workspace.drafts.take(&owner).expect("draft");
        draft.content.files.push(DraftFile {
            id: AttachmentId::from("reference"),
            name: Some("source.txt".into()),
            media_type: Some("text/plain".into()),
            byte_len: None,
            source_document_id: None,
            content: DraftFileContent::Reference {
                uri: "file:///project/source.txt".into(),
            },
        });
        draft.content.context.push(DraftContext {
            id: ContextItemId::from("context"),
            text: "retained selection".into(),
            description: None,
            metadata_json: None,
            instructions: None,
        });
        workspace.drafts.save(owner, draft.clone());
        workspace.submit_from_enter(false, window, cx);
        assert_eq!(
            workspace.composer_draft(cx),
            draft,
            "busy queueing does not consume a text-only subset"
        );
        workspace.apply_server_events(broadcast(10, vec![item("queued", json!({}))]), window, cx);
        workspace
            .live_status
            .insert(harness.key.session.clone(), LiveStatus::Idle);
        workspace.submit_from_enter(false, window, cx);
        assert_eq!(
            workspace.composer_draft(cx),
            draft,
            "prefix merge does not consume a text-only subset"
        );
        assert_eq!(
            workspace.send_attempts.iter().count(),
            0,
            "no incomplete send was captured"
        );
        assert!(
            workspace.queue_storage.operations().is_empty(),
            "a reference-only file refuses before queue ownership is acquired"
        );
        workspace.start_background_save(cx);
    });
    cx.run_until_parked();
    assert_eq!(count(&harness.server, "/items"), 0);
    assert_eq!(count(&harness.server, "preview-prefix"), 0);
    assert_eq!(count(&harness.server, "reserve-prefix"), 0);
    assert_eq!(count(&harness.server, "/prompt"), 0);
}

#[gpui_kit::test]
fn a_healthy_queued_message_is_one_composer_row_without_a_recovery_row(cx: &mut TestAppContext) {
    use gpui_kit::px;

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "follow up", false);
    // No frame of the enqueue draws a recovery row: not while it is captured
    // and submitted, not between its acceptance and the queue's reread, and
    // not once it is accepted, queued by its receipt and listed by its queue.
    harness.frames_until(cx, false, |cx| {
        harness.queued(cx) == ["follow up"]
            && harness.composer(cx).is_empty()
            && cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                let operations = workspace.queue_storage.operations();
                !workspace.queue.enqueues.any_busy()
                    && operations.len() == 1
                    && workspace.queue.enqueues.seen_listed(operations[0].id())
                    && !workspace
                        .queue
                        .enqueues
                        .awaiting_listing(operations[0].id())
            })
    });
    harness.idle(cx);
    harness.frame(cx, false);
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        let (operation, id) = {
            let workspace = harness.workspace.read(cx);
            assert!(
                matches!(
                    workspace.queue_storage.operations()[0].action(),
                    crate::queue_storage::QueueAction::Enqueue {
                        phase: crate::queue_storage::QueueEnqueuePhase::Accepted { .. },
                        ..
                    }
                ),
                "the enqueue stays held for a later check"
            );
            assert!(
                workspace.enqueue_recovery_items().is_empty(),
                "a healthy queued message has nothing to recover"
            );
            (
                workspace.queue_storage.operations()[0].id().clone(),
                workspace.selected_queue()[0].id.as_str().to_owned(),
            )
        };
        assert!(
            window
                .try_find(gpui_kit::ElementId::Name(
                    format!("queue-enqueue-{}", operation.as_str()).into()
                ))
                .is_none(),
            "no recovery row is drawn"
        );
        let named = |name: &str| gpui_kit::ElementId::Name(format!("queued-{name}-{id}").into());
        let strip = window.find("queued-strip").bounds();
        let row = window.find(named("row")).bounds();
        let remove = window.find(named("remove")).bounds();
        assert_eq!(
            strip.size.height,
            px(32.0),
            "`.oc-composer-row` is 2rem with the strip's border inside it"
        );
        assert_eq!(row.size.height, px(31.0), "the row above the 1px border");
        assert_eq!(
            window.find("composer").bounds().size.height,
            px(132.0),
            "the queue row over the resting 100px composer, as the web's box"
        );
        assert_eq!(
            remove.size,
            gpui_kit::size(px(32.0), px(28.0)),
            "a small Button around a 16px icon: h-7 and px-2"
        );
        assert_eq!(row.right() - remove.right(), px(6.0), "the row's pe-1.5");
        let send = window.find(named("send")).bounds();
        let edit = window.find(named("edit")).bounds();
        assert_eq!(send.size.height, px(28.0));
        assert_eq!(edit.size.height, px(28.0));
        assert_eq!(remove.left() - send.right(), px(8.0), "the row's gap-2");
        assert_eq!(send.left() - edit.right(), px(8.0), "the row's gap-2");
    })
    .expect("window");
}

#[gpui_kit::test]
fn a_queue_row_removed_by_the_user_leaves_no_recovery_row(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "removed by hand", false);
    harness.frames_until(cx, false, |cx| {
        harness.queued(cx) == ["removed by hand"]
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let item = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    let lookups = harness.lookups();
    harness.act(cx, |workspace, _, cx| workspace.remove_queued(&item, cx));
    harness.frames_until(cx, false, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            workspace.queue_storage.operations().is_empty() && !workspace.queue.enqueues.any_busy()
        })
    });
    assert!(harness.queued(cx).is_empty());
    assert_eq!(
        harness.lookups(),
        lookups + 1,
        "the removal is observed by one read, after the server answered it"
    );
    assert_eq!(
        count(&harness.server, "/enqueue"),
        1,
        "observing the removal never resubmits"
    );
}

#[gpui_kit::test]
fn a_failed_automatic_check_shows_its_row_once(cx: &mut TestAppContext) {
    use crate::queue_enqueue::EnqueueCheck;

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "checked once", false);
    harness.frames_until(cx, false, |cx| {
        harness.queued(cx) == ["checked once"]
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let id = cx.read(|cx| {
        harness.workspace.read(cx).queue_storage.operations()[0]
            .id()
            .clone()
    });
    {
        let mut state = harness.server.state.lock().expect("state");
        state
            .enqueue_requests
            .get_mut(id.as_str())
            .expect("immutable receipt")["state"] = json!("dispatched");
        state.items.clear();
        state.revision += 1;
        // The lookup of this one request answers 500.
        state
            .fail
            .push(Box::leak(id.as_str().to_owned().into_boxed_str()));
    }
    let lookups = harness.lookups();
    harness.reread(cx);
    harness.frames_until(cx, true, |cx| {
        harness.lookups() == lookups + 1
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let rows = cx.read(|cx| harness.workspace.read(cx).enqueue_recovery_items());
    assert_eq!(rows.len(), 1, "the failed check leaves the row");
    assert!(rows[0].id == id);
    assert!(
        rows[0].status == EnqueueCheck::Failed,
        "the row says its check failed"
    );
    assert!(rows[0].can_check, "the row offers the manual check");
    // Neither another reread nor further frames check again.
    harness.reread(cx);
    for _ in 0..3 {
        harness.frame(cx, true);
        cx.run_until_parked();
    }
    harness.frames_until(cx, true, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.any_payload_busy())
    });
    assert_eq!(
        harness.lookups(),
        lookups + 1,
        "one automatic check for one departure"
    );
    assert_eq!(
        cx.read(|cx| harness.workspace.read(cx).enqueue_recovery_items().len()),
        1
    );
    // The manual check on that row still observes the delivery.
    harness.server.state.lock().expect("state").fail.clear();
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-check", id.as_str()).into()),
            cx,
        );
    })
    .expect("the row's own control");
    harness.frames_until(cx, true, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            workspace.queue_storage.operations().is_empty() && !workspace.queue.enqueues.any_busy()
        })
    });
    assert_eq!(harness.lookups(), lookups + 2);
}

#[gpui_kit::test]
fn a_failed_host_queue_read_shows_the_row(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "kept after a failed read", false);
    harness.frames_until(cx, false, |cx| {
        harness.queued(cx) == ["kept after a failed read"]
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let id = cx.read(|cx| {
        harness.workspace.read(cx).queue_storage.operations()[0]
            .id()
            .clone()
    });
    // Every read of the host's queue fails from here on.
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("message-queue");
    close(cx, harness.window, &harness.workspace);
    let (window, workspace) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_queue_session(workspace, &harness.key, window, cx)
        });
    })
    .expect("reopened");
    let rows = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .enqueue_recovery_items()
                .iter()
                .filter(|row| row.id == id)
                .count()
        })
    };
    wait(cx, |cx| rows(cx) == 1);
    assert_eq!(
        rows(cx),
        1,
        "a queue that was never read, or could not be, hides nothing"
    );
    assert_eq!(
        harness.lookups(),
        0,
        "the row is manual: hydration and a failed read start no check"
    );
    // While a read is on its way the receipt still stands.
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |workspace, cx| workspace.read_queue_owner(None, cx));
    })
    .expect("window");
    assert_eq!(rows(cx), 0, "an unanswered read is not a failed one");
    wait(cx, |cx| rows(cx) == 1);
    // A read that answers lists the message, and the row goes.
    harness.server.state.lock().expect("state").fail.clear();
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |workspace, cx| workspace.read_queue_owner(None, cx));
    })
    .expect("window");
    wait(cx, |cx| {
        harness.queued_in(cx, &workspace) == ["kept after a failed read"]
    });
    assert_eq!(rows(cx), 0);
    assert_eq!(harness.lookups(), 0);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn enter_queues_while_busy_with_the_captured_selection(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "\nfollow up\n", false);
    assert_eq!(
        harness.composer(cx),
        "\nfollow up\n",
        "the original stays visible until its complete source is durable"
    );
    wait(cx, |cx| {
        harness.queued(cx) == ["\nfollow up\n"] && harness.composer(cx).is_empty()
    });
    let body = harness
        .server
        .state
        .lock()
        .expect("state")
        .captures
        .values()
        .next()
        .expect("payload capture")
        .clone();
    assert_eq!(body["directory"], "/project");
    assert_eq!(
        body["item"]["content"], "\nfollow up\n",
        "the captured native source remains exact"
    );
    assert_eq!(
        body["item"]["sendConfig"],
        json!({"providerID": "provider", "modelID": "model", "agent": "agent", "variant": "effort"})
    );
    assert!(
        !harness
            .server
            .calls()
            .iter()
            .any(|call| call.ends_with("/prompt")),
        "nothing is sent while the turn runs"
    );
    assert_eq!(
        count(&harness.server, "/items"),
        0,
        "composer enqueue never calls the destructive legacy protocol"
    );
    let requests = harness.server.requests.lock().expect("requests");
    let enqueue = requests
        .iter()
        .find(|request| request.path.ends_with("/enqueue"))
        .expect("durable enqueue");
    let journal = enqueue.journal.as_ref().expect("journal precedes POST");
    assert!(journal["queue"]["operations"][0]["action"]["enqueue"]["phase"]["unknown"].is_object());
    assert!(
        journal["queue"]["operations"][0]["action"]["enqueue"]["source"]["companion"].is_string()
    );
}

#[gpui_kit::test]
fn an_unknown_enqueue_keeps_its_source_and_retries_identically_from_the_recovery_button(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("enqueue");
    harness.type_and_enter(cx, "lost?", false);
    wait(cx, |_| count(&harness.server, "/enqueue") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("newer", window, cx));
    });
    assert_eq!(
        harness.composer(cx),
        "newer",
        "unknown ownership never becomes a second draft submission"
    );
    assert!(harness.queued(cx).is_empty());
    let id = cx.read(|cx| {
        harness.workspace.read(cx).queue_storage.operations()[0]
            .id()
            .clone()
    });
    harness.act(cx, |workspace, window, cx| {
        workspace.queue_composer(window, cx);
    });
    assert_eq!(
        count(&harness.server, "/enqueue"),
        1,
        "another enqueue identity stays blocked"
    );
    harness.server.state.lock().expect("state").fail.clear();
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-check", id.as_str()).into()),
            cx,
        );
    })
    .expect("read-only recovery button");
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            !workspace.queue.enqueues.any_busy()
                && workspace.queue.enqueues.check(&id)
                    == crate::queue_enqueue::EnqueueCheck::NotObserved
        })
    });
    assert_eq!(
        count(&harness.server, "/enqueue"),
        1,
        "a missing status lookup never authorizes submission"
    );
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-retry", id.as_str()).into()),
            cx,
        );
    })
    .expect("recovery button");
    wait(cx, |cx| harness.queued(cx) == ["lost?"]);
    assert_eq!(harness.composer(cx), "newer");
    let requests = harness.server.requests.lock().expect("requests");
    let submissions: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/enqueue"))
        .collect();
    assert_eq!(submissions.len(), 2);
    assert_eq!(
        submissions[0].path, submissions[1].path,
        "same request identity"
    );
    assert_eq!(
        submissions[0].body, submissions[1].body,
        "identical retained request body"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "PUT" && request.path.contains("/attachments/"))
            .count(),
        1,
        "ready recovery does not choose another upload identity"
    );
}

#[gpui_kit::test]
fn a_dropped_enqueue_reply_is_only_observed_after_restart_until_explicit_activation(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness
        .server
        .state
        .lock()
        .expect("state")
        .drop_enqueue_reply = true;
    harness.type_and_enter(cx, "original queue payload", false);
    wait(cx, |_| count(&harness.server, "/enqueue") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let id = cx.read(|cx| {
        harness.workspace.read(cx).queue_storage.operations()[0]
            .id()
            .clone()
    });
    let uploads = count(&harness.server, "/attachments/");
    close(cx, harness.window, &harness.workspace);
    let (window, workspace) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_queue_session(workspace, &harness.key, window, cx)
        });
    })
    .expect("reopened");
    cx.run_until_parked();
    assert_eq!(
        count(&harness.server, "/enqueue"),
        1,
        "hydration never repeats enqueue"
    );
    assert_eq!(
        count(&harness.server, "/attachments/"),
        uploads,
        "hydration never uploads"
    );
    assert!(
        !harness
            .server
            .calls()
            .iter()
            .any(|call| call.starts_with("GET ") && call.contains("/requests/")),
        "hydration never looks up queue operations"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-check", id.as_str()).into()),
            cx,
        );
    })
    .expect("actual status button");
    wait(cx, |cx| {
        cx.read(|cx| {
            matches!(
                workspace
                    .read(cx)
                    .queue_storage
                    .operation(&id)
                    .expect("original enqueue")
                    .action(),
                crate::queue_storage::QueueAction::Enqueue {
                    phase: crate::queue_storage::QueueEnqueuePhase::Accepted { .. },
                    ..
                }
            ) && !workspace.read(cx).queue.enqueues.any_busy()
        })
    });
    assert_eq!(
        count(&harness.server, "/enqueue"),
        1,
        "status checks never resubmit"
    );
    assert_eq!(count(&harness.server, "/attachments/"), uploads);
    assert_eq!(
        harness.server.state.lock().expect("state").items.len(),
        1,
        "lost receipt never creates a second item"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn explicit_delivery_retry_is_journaled_and_a_lost_reply_does_not_authorize_another_attempt(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "same delivery", false);
    wait(cx, |cx| {
        harness.queued(cx) == ["same delivery"]
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let id = cx.read(|cx| {
        harness.workspace.read(cx).queue_storage.operations()[0]
            .id()
            .clone()
    });
    {
        let mut state = harness.server.state.lock().expect("state");
        state
            .enqueue_requests
            .get_mut(id.as_str())
            .expect("original receipt")["state"] = json!("uncertain");
    }
    let click = |suffix: &str, cx: &mut TestAppContext| {
        cx.update_window(harness.window, |_, window, cx| {
            window.render_frame(cx);
            window.click(
                gpui_kit::ElementId::Name(format!("queue-enqueue-{}-{suffix}", id.as_str()).into()),
                cx,
            );
        })
        .expect("actual recovery control");
    };
    harness.reread_until_row(cx, &id);
    let lookups = harness.lookups();
    click("check", cx);
    // The listing already offers Retry; these wait for the Check's own read.
    wait(cx, |_| harness.lookups() == lookups + 1);
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            !workspace.queue.enqueues.any_busy()
                && workspace.queue.enqueues.delivery(&id) == Some(QueueEnqueueState::Uncertain)
                && workspace
                    .enqueue_recovery_items()
                    .iter()
                    .any(|row| row.id == id && row.can_retry)
        })
    });
    harness.server.state.lock().expect("state").drop_retry_reply = true;
    click("retry", cx);
    wait(cx, |_| count(&harness.server, "/retry") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    {
        let requests = harness.server.requests.lock().expect("requests");
        let retry = requests
            .iter()
            .find(|request| request.path.ends_with("/retry"))
            .expect("explicit retry");
        assert_eq!(
            serde_json::from_slice::<Value>(&retry.body).expect("retry body"),
            json!({"directory":"/project","attempt":1})
        );
        let phase = &retry.journal.as_ref().expect("saved authorization")["queue"]["operations"][0]
            ["action"]["enqueue"]["phase"];
        assert!(
            phase["retry_prepared"].is_object(),
            "retry intent precedes its network authorization"
        );
    }
    harness
        .server
        .state
        .lock()
        .expect("state")
        .enqueue_requests
        .get_mut(id.as_str())
        .expect("retained receipt")["state"] = json!("uncertain");
    close(cx, harness.window, &harness.workspace);
    let (window, workspace) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &workspace);
    let lookups = count(
        &harness.server,
        "GET /api/message-queue/sessions/ses_queue/requests/",
    );
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_queue_session(workspace, &harness.key, window, cx)
        });
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-retry", id.as_str()).into()),
            cx,
        );
    })
    .expect("retry after restart");
    wait(cx, |_| {
        count(
            &harness.server,
            "GET /api/message-queue/sessions/ses_queue/requests/",
        ) == lookups + 1
    });
    wait(cx, |cx| {
        cx.read(|cx| !workspace.read(cx).queue.enqueues.any_busy())
    });
    assert_eq!(
        count(&harness.server, "/retry"),
        1,
        "lookup recovers the already-authorized attempt"
    );
    assert_eq!(
        harness.server.state.lock().expect("state").enqueue_requests[id.as_str()]["attempt"],
        1
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-retry", id.as_str()).into()),
            cx,
        );
    })
    .expect("another explicit retry activation");
    wait(cx, |_| count(&harness.server, "/retry") == 2);
    wait(cx, |cx| {
        cx.read(|cx| !workspace.read(cx).queue.enqueues.any_busy())
    });
    assert_eq!(
        harness.server.body_of("/retry")["attempt"],
        2,
        "only another user activation authorizes the next attempt"
    );
    assert_eq!(count(&harness.server, "/enqueue"), 1);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn context_only_enter_captures_all_semantic_content_without_a_text_placeholder(
    cx: &mut TestAppContext,
) {
    use openchamber_native_core::draft_content::DraftContext;
    use openchamber_native_core::ids::ContextItemId;

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        let owner = workspace.composer_key.clone().expect("native owner");
        let mut draft = Draft::default();
        draft.content.context.push(DraftContext {
            id: ContextItemId::from("original-context"),
            text: "complete selected context".into(),
            description: Some("selection".into()),
            metadata_json: Some(r#"{"source":"selection"}"#.into()),
            instructions: Some("keep this instruction first".into()),
        });
        workspace.drafts.save(owner, draft);
        workspace.submit_from_enter(false, window, cx);
    });
    wait(cx, |_| count(&harness.server, "/enqueue") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let state = harness.server.state.lock().expect("state");
    let capture = state.captures.values().next().expect("complete payload");
    assert_eq!(
        capture["item"]["promptParts"]["context"][0]["text"],
        "keep this instruction first"
    );
    assert_eq!(
        capture["item"]["promptParts"]["context"][1]["text"],
        "complete selected context"
    );
    assert_eq!(
        capture["item"]["promptParts"]["context"][1]["metadata"],
        json!({"source":"selection"})
    );
    drop(state);
    assert!(
        cx.read(|cx| harness.workspace.read(cx).composer_draft(cx).is_empty()),
        "the durable matching capture consumes its semantic chips"
    );
}

#[gpui_kit::test]
fn terminal_enqueue_status_retires_only_its_local_capture_after_observation(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "original completed delivery", false);
    wait(cx, |cx| {
        harness.queued(cx) == ["original completed delivery"]
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let (id, labels) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let id = workspace.queue_storage.operations()[0].id().clone();
        let labels: Vec<_> = workspace
            .queue_storage
            .objects()
            .iter()
            .flat_map(|object| object.reference().chunks())
            .map(|chunk| {
                chunk
                    .descriptor()
                    .expect("local descriptor")
                    .id()
                    .as_str()
                    .to_owned()
            })
            .collect();
        (id, labels)
    });
    let spool = harness
        ._fixture
        .journal()
        .parent()
        .expect("profile")
        .join("attachment-spool");
    assert!(
        labels
            .iter()
            .all(|label| spool.join(format!("blob-{label}.bin")).is_file()),
        "queued delivery retains the verified local captures"
    );
    {
        let mut state = harness.server.state.lock().expect("state");
        state
            .enqueue_requests
            .get_mut(id.as_str())
            .expect("immutable receipt")["state"] = json!("dispatched");
        state.items.clear();
        state.revision += 1;
    }
    // The delivered message leaves its queue. Its departure is checked once,
    // without a recovery row in any frame.
    let lookups = harness.lookups();
    harness.reread(cx);
    harness.frames_until(cx, false, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            workspace.queue_storage.operation(&id).is_none() && !workspace.queue.enqueues.any_busy()
        })
    });
    assert_eq!(
        harness.lookups(),
        lookups + 1,
        "one read of the original request observes the delivery"
    );
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            workspace.queue_storage.operations().is_empty()
                && workspace
                    .queue_storage
                    .retired_blobs()
                    .expect("named retirement intents")
                    .is_empty()
                && !workspace.queue.enqueues.any_busy()
        })
    });
    assert!(
        labels
            .iter()
            .all(|label| !spool.join(format!("blob-{label}.bin")).exists()),
        "durable terminal observation permits local chunk retirement"
    );
    assert!(
        !harness
            .server
            .calls()
            .iter()
            .any(|call| call.starts_with("DELETE ") && call.contains("/attachments/")),
        "native local retirement never releases server uploads"
    );
}

#[cfg(unix)]
#[gpui_kit::test]
fn explicit_enqueue_retry_repairs_a_partial_spool_write_without_changing_custody(
    cx: &mut TestAppContext,
) {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    use crate::attachment_spool::BlobId;
    use crate::queue_capture::{
        QueueCaptureBytes, QueueNativeSource, QueueObjectId, QueueObjectKind, QueueOperationId,
    };
    use crate::queue_storage::{QueueConfiguration, QueueObject, QueueOperation, QueueOwner};

    let harness = harness(cx, FollowUpBehavior::Queue);
    let operation = QueueOperationId::new("enqueue_partial_original").expect("original request");
    let object =
        QueueObjectId::new("enqueue_partial_original-source").expect("original source object");
    let blob = BlobId::parse("00000000000000000000000000000201").expect("original local label");
    let owner = cx
        .read(|cx| harness.workspace.read(cx).composer_key.clone())
        .expect("composer owner");
    let source = QueueNativeSource {
        owner: owner.clone(),
        draft: Draft::new("source kept through partial write"),
    };
    let capture = QueueCaptureBytes::native_source(&source, &[blob]).expect("complete source plan");
    let configuration = openchamber_native_core::message_queue::QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "model".into(),
        agent: Some("agent".into()),
        variant: Some("effort".into()),
    };
    harness.act(cx, |workspace, window, cx| {
        workspace.drafts.save(owner.clone(), source.draft.clone());
        workspace.restore_composer_draft(window, cx);
        let captured_owner =
            QueueOwner::new(owner.runtime(), "/project", &harness.key.session, &owner)
                .expect("exact original owner");
        let captured = QueueOperation::enqueue(
            operation.clone(),
            captured_owner,
            QueueConfiguration::capture(&configuration),
            source.clone(),
        )
        .expect("complete source capture");
        workspace
            .change_queue_storage(|queue, workspace| {
                queue
                    .admit_operation(captured, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                    .map_err(|(error, _)| error)?;
                queue
                    .admit_object(
                        QueueObject::planned(
                            object.clone(),
                            operation.clone(),
                            QueueObjectKind::NativeCompanion,
                            1,
                            capture.reference().clone(),
                        )?,
                        |candidate| workspace.validate_queue_candidate(candidate),
                    )
                    .map_err(|(error, _)| error)
            })
            .expect("canonical full-snapshot admission");
        workspace.start_background_save(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|snapshot| {
                snapshot["queue"]["objects"]
                    .as_array()
                    .is_some_and(|objects| {
                        objects.iter().any(|saved| saved["id"] == object.as_str())
                    })
            })
    });
    let path = harness
        ._fixture
        .journal()
        .parent()
        .expect("profile")
        .join("attachment-spool")
        .join(format!("blob-{}.bin", blob.as_str()));
    let prefix = &capture.bytes()[..19];
    let mut partial = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("private partial file");
    partial.write_all(prefix).expect("surviving short write");
    partial.sync_all().expect("persist partial artifact");
    drop(partial);
    let inode = fs::metadata(&path).expect("partial inode").ino();
    close(cx, harness.window, &harness.workspace);
    let (window, workspace) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_queue_session(workspace, &harness.key, window, cx)
        });
    })
    .expect("reopened original owner");
    cx.run_until_parked();
    assert_eq!(
        fs::read(&path).expect("unreplayed partial"),
        prefix,
        "hydration never repairs held chunks"
    );
    assert_eq!(count(&harness.server, "/enqueue"), 0);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-enqueue-{}-retry", operation.as_str()).into()),
            cx,
        );
    })
    .expect("explicit retry control");
    wait(cx, |_| count(&harness.server, "/enqueue") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !workspace.read(cx).queue.enqueues.any_busy())
    });
    assert_eq!(
        fs::metadata(&path).expect("repaired same label").ino(),
        inode
    );
    assert_eq!(
        fs::read(&path).expect("repaired full source"),
        capture.bytes()
    );
    let retained = cx.read(|cx| {
        workspace.read(cx).queue_storage.operations()[0]
            .id()
            .clone()
    });
    assert_eq!(
        retained, operation,
        "repair retains the original server operation identity"
    );
    let state = harness.server.state.lock().expect("state");
    assert_eq!(
        state.captures[operation.as_str()]["item"]["text"],
        source.draft.text
    );
    assert_eq!(
        state.captures[operation.as_str()]["item"]["sendConfig"]["variant"],
        "effort"
    );
    drop(state);
    close(cx, window, &workspace);
}

fn check_terminal_enqueue_without_local_payload(
    cx: &mut TestAppContext,
    retry_prepared: bool,
    missing: bool,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "accepted server custody", false);
    wait(cx, |cx| {
        harness.queued(cx) == ["accepted server custody"]
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let (id, payload) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let operation = &workspace.queue_storage.operations()[0];
        let crate::queue_storage::QueueAction::Enqueue {
            phase: crate::queue_storage::QueueEnqueuePhase::Accepted { payload, .. },
            ..
        } = operation.action()
        else {
            panic!("accepted receipt");
        };
        let blob = workspace
            .queue_storage
            .object(payload)
            .expect("payload object")
            .reference()
            .chunks()[0]
            .descriptor()
            .expect("payload chunk");
        (operation.id().clone(), *blob.id())
    });
    let click = |action: &str, cx: &mut TestAppContext| {
        cx.update_window(harness.window, |_, window, cx| {
            window.render_frame(cx);
            window.click(
                gpui_kit::ElementId::Name(format!("queue-enqueue-{}-{action}", id.as_str()).into()),
                cx,
            );
        })
        .expect("actual recovery control");
    };
    if retry_prepared {
        harness
            .server
            .state
            .lock()
            .expect("state")
            .enqueue_requests
            .get_mut(id.as_str())
            .expect("original receipt")["state"] = json!("uncertain");
        harness.reread_until_row(cx, &id);
        let lookups = harness.lookups();
        click("check", cx);
        // The listing already offers Retry; these wait for the Check's own read.
        wait(cx, |_| harness.lookups() == lookups + 1);
        wait(cx, |cx| {
            cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                !workspace.queue.enqueues.any_busy()
                    && workspace.queue.enqueues.delivery(&id) == Some(QueueEnqueueState::Uncertain)
                    && workspace
                        .enqueue_recovery_items()
                        .iter()
                        .any(|row| row.id == id && row.can_retry)
            })
        });
        harness.server.state.lock().expect("state").drop_retry_reply = true;
        click("retry", cx);
        wait(cx, |_| count(&harness.server, "/retry") == 1);
        wait(cx, |cx| {
            cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
        });
        assert!(cx.read(|cx| {
            matches!(
                harness
                    .workspace
                    .read(cx)
                    .queue_storage
                    .operation(&id)
                    .expect("original operation")
                    .action(),
                crate::queue_storage::QueueAction::Enqueue {
                    phase: crate::queue_storage::QueueEnqueuePhase::RetryPrepared { .. },
                    ..
                }
            )
        }));
    }
    let path = harness
        ._fixture
        .journal()
        .parent()
        .expect("profile")
        .join("attachment-spool")
        .join(format!("blob-{}.bin", payload.as_str()));
    if missing {
        fs::remove_file(&path).expect("missing local payload fixture");
    } else {
        fs::write(&path, b"corrupt local payload").expect("corrupt local payload fixture");
    }
    let submissions = count(&harness.server, "/enqueue");
    let uploads = count(&harness.server, "/attachments/");
    let retries = count(&harness.server, "/retry");
    harness
        .server
        .state
        .lock()
        .expect("state")
        .enqueue_requests
        .get_mut(id.as_str())
        .expect("same receipt")["state"] = json!("dispatched");
    let lookups = harness.lookups();
    if retry_prepared {
        // A prepared retry keeps its row; its Check is the click.
        click("check", cx);
    } else {
        // The queue stops vouching for the request, and its departure is
        // checked without a row or a click.
        harness.reread(cx);
    }
    harness.frames_until(cx, retry_prepared, |_| harness.lookups() == lookups + 1);
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            workspace.queue_storage.operation(&id).is_none()
                && !workspace.queue.enqueues.any_busy()
                && workspace
                    .queue_storage
                    .retired_blobs()
                    .expect("retirement intents")
                    .is_empty()
        })
    });
    assert_eq!(
        count(&harness.server, "/enqueue"),
        submissions,
        "status cannot resubmit accepted content"
    );
    assert_eq!(
        count(&harness.server, "/attachments/"),
        uploads,
        "status needs no local upload bytes"
    );
    assert_eq!(
        count(&harness.server, "/retry"),
        retries,
        "Check status never authorizes delivery"
    );
    close(cx, harness.window, &harness.workspace);
}

#[gpui_kit::test]
fn accepted_enqueue_status_and_retirement_survive_missing_or_corrupt_payload(
    cx: &mut TestAppContext,
) {
    for missing in [true, false] {
        check_terminal_enqueue_without_local_payload(cx, false, missing);
    }
}

#[gpui_kit::test]
fn prepared_retry_status_and_retirement_survive_missing_or_corrupt_payload(
    cx: &mut TestAppContext,
) {
    for missing in [true, false] {
        check_terminal_enqueue_without_local_payload(cx, true, missing);
    }
}

#[cfg(unix)]
#[gpui_kit::test]
fn retirement_releases_healthy_blobs_on_both_sides_of_a_failure_and_retries_only_the_failed_intent(
    cx: &mut TestAppContext,
) {
    use std::os::unix::fs::{OpenOptionsExt, symlink};

    let harness = harness(cx, FollowUpBehavior::Queue);
    let labels: Vec<_> = (301..=303).map(|value| format!("{value:032x}")).collect();
    let profile = harness
        ._fixture
        .journal()
        .parent()
        .expect("profile")
        .to_owned();
    let spool = profile.join("attachment-spool");
    let paths: Vec<_> = labels
        .iter()
        .map(|label| spool.join(format!("blob-{label}.bin")))
        .collect();
    for path in [&paths[0], &paths[2]] {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("retired private test blob");
        file.write_all(b"retired fixture bytes")
            .expect("fixture bytes");
    }
    let protected = profile.join("protected-test-artifact.bin");
    fs::write(&protected, b"protected independent fixture").expect("test-owned protected target");
    symlink(&protected, &paths[1]).expect("unsafe middle retired artifact");
    harness.act(cx, |workspace, _, cx| {
        // A loaded retirement snapshot is the durable authority left by a prior
        // terminal operation. No live enqueue or take owns any of these labels.
        let mut stored =
            serde_json::to_value(&workspace.queue_storage).expect("current ledger schema");
        stored["retired"] = json!(labels);
        let candidate = serde_json::from_value(stored).expect("retirement-only crash fixture");
        workspace
            .validate_queue_candidate(&candidate)
            .expect("canonical complete snapshot");
        workspace.queue_storage = candidate;
        workspace.start_background_save(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|saved| {
                saved["queue"]["retired"]
                    .as_array()
                    .is_some_and(|retired| retired.len() == 3)
            })
    });
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name("queue-enqueue-release".into()),
            cx,
        );
    })
    .expect("explicit local retirement control");
    wait(cx, |cx| {
        cx.read(|cx| {
            !harness.workspace.read(cx).queue.enqueues.any_busy()
                && harness
                    .workspace
                    .read(cx)
                    .queue_storage
                    .retired_blobs()
                    .expect("retained intents")
                    .len()
                    == 1
        })
    });
    let saved: Value =
        serde_json::from_slice(&fs::read(harness._fixture.journal()).expect("each success saved"))
            .expect("journal JSON");
    assert_eq!(saved["queue"]["retired"], json!([labels[1]]));
    assert!(
        !paths[0].exists() && !paths[2].exists(),
        "one unsafe blob does not block either independent neighbor"
    );
    assert!(
        fs::symlink_metadata(&paths[1])
            .expect("failed intent artifact")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(&protected).expect("protected target"),
        b"protected independent fixture"
    );
    // New unrelated files at already-released names must never enter the retry.
    for path in [&paths[0], &paths[2]] {
        fs::write(path, b"new unrelated artifact").expect("post-success fixture");
    }
    close(cx, harness.window, &harness.workspace);
    let (window, workspace) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &workspace);
    assert_eq!(
        cx.read(|cx| workspace
            .read(cx)
            .queue_storage
            .retired_blobs()
            .expect("restart retains only failure")
            .len()),
        1
    );
    fs::remove_file(&paths[1]).expect("repair test-owned symlink fixture");
    let mut repaired = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&paths[1])
        .expect("corrected failed test blob");
    repaired
        .write_all(b"retired fixture bytes")
        .expect("corrected bytes");
    drop(repaired);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name("queue-enqueue-release".into()),
            cx,
        );
    })
    .expect("explicit corrected retry");
    wait(cx, |cx| {
        cx.read(|cx| {
            !workspace.read(cx).queue.enqueues.any_busy()
                && workspace
                    .read(cx)
                    .queue_storage
                    .retired_blobs()
                    .expect("remaining intents")
                    .is_empty()
        })
    });
    assert!(!paths[1].exists(), "corrected failed intent releases once");
    for path in [&paths[0], &paths[2]] {
        assert_eq!(
            fs::read(path).expect("unrelated file remains"),
            b"new unrelated artifact"
        );
    }
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn prepared_file_bytes_and_newer_typing_keep_their_original_host_during_enqueue(
    cx: &mut TestAppContext,
) {
    use crate::attachment_intake::{AttachmentInputSource, AttachmentIntake};

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        let owner = workspace.composer_key.clone().expect("source owner");
        assert!(
            workspace
                .ingest_attachments(
                    owner,
                    vec![AttachmentIntake {
                        source: AttachmentInputSource::Bytes(b"original prepared bytes".to_vec()),
                        filename: "notes.txt".into(),
                        declared_media_type: None,
                        reserved_filenames: Vec::new(),
                    }],
                    window,
                    cx
                )
                .is_empty()
        );
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            let owner = workspace.composer_key.as_ref().expect("owner");
            !workspace.attachment_storage.has_pending(owner)
                && !workspace.attachments.is_empty()
                && workspace.attachment_preparations.is_empty()
                && workspace.persistence_ready()
        })
    });
    harness.server.state.lock().expect("state").hold = Some("payload-0");
    harness.type_and_enter(cx, "with full file", false);
    wait(cx, |_| {
        count(&harness.server, "/attachments/payload-0") == 1
    });
    let original = cx
        .read(|cx| harness.workspace.read(cx).composer_key.clone())
        .expect("original owner");
    harness.act(cx, |workspace, window, cx| {
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value("newer text", window, cx)
        });
        workspace.save_composer_draft(cx);
        workspace.connect_to_host(
            Endpoint::parse("http://127.0.0.1:1/").expect("other endpoint"),
            window,
            cx,
        );
        workspace.switch_runtime(RuntimeId::from("replacement-host"), window, cx);
        workspace.release_retired_networks();
        assert_eq!(
            workspace.retired_networks.len(),
            1,
            "queue ownership retains its original executor"
        );
    });
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |_| count(&harness.server, "/enqueue") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let state = harness.server.state.lock().expect("state");
    let (_, bytes) = state
        .uploads
        .iter()
        .find(|(key, _)| key.ends_with("/file-0"))
        .expect("original raw file upload")
        .1;
    assert_eq!(bytes, b"original prepared bytes");
    let capture = state.captures.values().next().expect("original capture");
    assert_eq!(capture["item"]["text"], "with full file");
    assert_eq!(
        capture["item"]["attachmentManifest"][0]["filename"],
        "notes.txt"
    );
    drop(state);
    assert_eq!(
        cx.read(|cx| harness
            .workspace
            .read(cx)
            .drafts
            .get(&original)
            .expect("original draft")
            .text
            .clone()),
        "newer text"
    );
    assert!(
        harness.queued(cx).is_empty(),
        "old-host projection never enters the replacement runtime"
    );
}

#[gpui_kit::test]
fn enter_with_original_model_path_uploads_lossless_manifest_before_refused_enqueue(
    cx: &mut TestAppContext,
) {
    use openchamber_native_core::draft::{DraftKey, DraftTarget};
    use openchamber_native_core::draft_content::DraftContext;
    use openchamber_native_core::ids::{ContextItemId, SessionId};
    use openchamber_native_transport::queue_manifest::{
        QueuePayloadDocument, QueuePayloadItem, QueuePayloadUse,
    };

    use crate::attachment_intake::{AttachmentInputSource, AttachmentIntake};
    use crate::composer_payload::PreparedComposerInput;
    use crate::queue_capture::sha256;
    use crate::queue_storage::{EnqueueSourceCapture, QueueAction, QueueEnqueuePhase, QueueSource};

    let harness = harness(cx, FollowUpBehavior::Queue);
    let model_name = "/original/source/notes.txt";
    let text = "model path follow-up";
    let sibling_draft = Draft::new("independent sibling stays intact");
    let owner = cx
        .read(|cx| harness.workspace.read(cx).composer_key.clone())
        .expect("original draft owner");
    let sibling = DraftKey::new(
        owner.runtime().clone(),
        "/project",
        DraftTarget::Session(SessionId::from("ses_independent")),
    )
    .expect("independent draft owner");
    harness.act(cx, |workspace, window, cx| {
        assert!(
            workspace
                .ingest_attachments(
                    owner.clone(),
                    vec![AttachmentIntake {
                        source: AttachmentInputSource::Bytes(b"original".to_vec()),
                        filename: "notes.txt".into(),
                        declared_media_type: None,
                        reserved_filenames: Vec::new(),
                    }],
                    window,
                    cx
                )
                .is_empty()
        );
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            !workspace.attachment_storage.has_pending(&owner)
                && !workspace.attachments.is_empty()
                && workspace.attachment_preparations.is_empty()
                && workspace.persistence_ready()
        })
    });
    harness.act(cx, |workspace, window, cx| {
        workspace.save_composer_draft(cx);
        let mut draft = workspace
            .drafts
            .take(&owner)
            .expect("real intake published its draft");
        assert_eq!(draft.content.files.len(), 1);
        let file = &mut draft.content.files[0];
        let prepared = workspace
            .attachments
            .get(&file.id)
            .expect("real prepared-byte catalog entry");
        assert!(
            prepared.staging.is_none(),
            "this identity change must not contradict a retained staging filename"
        );
        assert_eq!(prepared.blob.byte_len(), 8);
        assert_eq!(prepared.blob.sha256(), &sha256(b"original"));
        file.name = Some(model_name.into());
        draft.edit_text(text.into(), 0..0);
        draft.content.context.push(DraftContext {
            id: ContextItemId::from("model-path-context"),
            text: "captured selected context".into(),
            description: Some("original selection".into()),
            metadata_json: Some(r#"{"source":"selection","keep":[1,true,"λ"]}"#.into()),
            instructions: Some("read this instruction first".into()),
        });
        draft.content.metadata_json = Some(r#"{"pathProof":{"literal":true}}"#.into());
        workspace.drafts.save(owner.clone(), draft);
        workspace
            .drafts
            .save(sibling.clone(), sibling_draft.clone());
        workspace.restore_composer_draft(window, cx);
        workspace
            .validate_queue_candidate(&workspace.queue_storage)
            .expect("the model-facing path is valid canonical stored source metadata");
        workspace.start_background_save(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|snapshot| snapshot.to_string().contains(model_name))
    });
    let (file_id, descriptor, original_content) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let draft = workspace.drafts.get(&owner).expect("durable source draft");
        let file = &draft.content.files[0];
        (
            file.id.clone(),
            workspace
                .attachments
                .get(&file.id)
                .expect("unchanged byte owner")
                .blob,
            draft.content.clone(),
        )
    });
    harness.act(cx, |workspace, _, cx| {
        assert!(workspace.persistence_ready() && !workspace.close_in_progress());
        assert_eq!(workspace.composer_key.as_ref(), Some(&owner));
        assert!(workspace.composer_document_ready(&owner));
        assert!(
            !workspace.enqueue_blocks_owner(&owner)
                && !workspace.queue_preview_pending(&harness.key.session)
        );
        assert_eq!(workspace.config_directory().as_deref(), Some("/project"));
        let catalog = workspace
            .send_catalog
            .directory("/project")
            .expect("the harness installed its authoritative catalog");
        assert!(
            catalog.agents.ready().is_some()
                && catalog.providers.ready().is_some()
                && catalog.opencode.ready().is_some()
        );
        let record = workspace
            .selected_session()
            .expect("the harness selected its real session record");
        assert_eq!(record.id, harness.key.session);
        assert_eq!(
            record.model,
            Some(ModelChoice {
                provider_id: "provider".into(),
                model_id: "model".into(),
                variant: Some("effort".into())
            })
        );
        assert!(
            openchamber_native_core::send_config::find_model(
                catalog.providers.ready().expect("authoritative providers"),
                record.model.as_ref().expect("record model")
            )
            .is_some(),
            "the authoritative catalog contains the session's model"
        );
        assert!(
            workspace.composer_editable(),
            "normal selection reconciliation is allowed for this ready owner"
        );
        assert!(
            workspace
                .send_selection()
                .is_some_and(|(_, selection)| selection.model.is_some()),
            "the entrypoint has its real captured model"
        );
        assert!(
            workspace
                .connection
                .as_ref()
                .is_some_and(|connection| connection.state == ConnectionState::Connected)
        );
        assert_eq!(
            workspace.composer_draft(cx).content,
            original_content,
            "the actual composer retains the valid prepared source"
        );
    });
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("enqueue");
    harness.type_and_enter(cx, text, false);
    wait(cx, |cx| {
        cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let requests = harness
        .server
        .requests
        .lock()
        .expect("actual HTTP observations");
    let file_puts = requests
        .iter()
        .filter(|request| {
            request.method == "PUT"
                && request
                    .path
                    .split('?')
                    .next()
                    .is_some_and(|path| path.ends_with("/attachments/file-0"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        file_puts.len(),
        1,
        "the actual Enter entrypoint must start its actor and upload the model-path attachment"
    );
    assert_eq!(file_puts[0].body, b"original");
    let payload_puts = requests
        .iter()
        .filter(|request| request.method == "PUT" && request.path.contains("/attachments/payload-"))
        .collect::<Vec<_>>();
    assert_eq!(
        payload_puts.len(),
        1,
        "the actual actor uploads its complete small structured capture once"
    );
    let posts = requests
        .iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/enqueue"))
        .collect::<Vec<_>>();
    assert_eq!(
        posts.len(),
        1,
        "the final enqueue is attempted exactly once before the scripted refusal"
    );
    let post = posts[0];
    let body: Value = serde_json::from_slice(&post.body).expect("actual compact enqueue request");
    assert_eq!(body["directory"], "/project");
    assert_eq!(body["item"]["format"], "payload-v1");
    assert!(
        body["item"].get("payloadIndex").is_none()
            && body["item"].get("payloadIndexHash").is_none(),
        "new uploads grant no returned index authority"
    );
    let operation_id = post
        .path
        .strip_prefix("/api/message-queue/sessions/ses_queue/requests/")
        .and_then(|tail| tail.strip_suffix("/enqueue"))
        .expect("captured enqueue operation identity");
    let request_root = format!("/api/message-queue/sessions/ses_queue/requests/{operation_id}");
    assert_eq!(
        file_puts[0].path,
        format!("{request_root}/attachments/file-0?directory=%2Fproject")
    );
    assert_eq!(
        payload_puts[0].path,
        format!("{request_root}/attachments/payload-0?directory=%2Fproject")
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.starts_with(&request_root))
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        ["PUT", "PUT", "POST"],
        "initial submission keeps upload and admission order"
    );
    let state = harness.server.state.lock().expect("scripted upload store");
    let (uploaded_file, uploaded_bytes) = &state.uploads[&format!("{operation_id}/file-0")];
    assert_eq!(uploaded_file["filename"], "notes.txt");
    assert_eq!(uploaded_file["mimeType"], "text/plain");
    assert_eq!(uploaded_bytes, b"original");
    assert!(
        uploaded_file.get("agentPath").is_none(),
        "immutable byte descriptors contain no model-facing override"
    );
    let digest = |bytes: &[u8]| {
        sha256(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    assert_eq!(
        uploaded_file["blobId"],
        digest(
            &serde_json::to_vec(&["ses_queue", "/project", operation_id, "file-0"])
                .expect("actual backend owner preimage")
        )
    );
    let mut capture_bytes = Vec::new();
    for (ordinal, chunk) in body["item"]["payloadManifest"]["chunks"]
        .as_array()
        .expect("ordered uploaded chunks")
        .iter()
        .enumerate()
    {
        assert_eq!(chunk["ordinal"], ordinal);
        assert_eq!(chunk["requestId"], operation_id);
        let attachment = chunk["attachmentId"]
            .as_str()
            .expect("captured chunk label");
        let (uploaded, bytes) = &state.uploads[&format!("{operation_id}/{attachment}")];
        let mut expected = uploaded.clone();
        expected["ordinal"] = json!(ordinal);
        assert_eq!(
            chunk, &expected,
            "each pointer entry retains its actual upload descriptor"
        );
        assert_eq!(chunk["size"], bytes.len());
        assert_eq!(chunk["sha256"], digest(bytes));
        capture_bytes.extend_from_slice(bytes);
    }
    assert_eq!(
        body["item"]["payloadManifest"]["totalBytes"],
        capture_bytes.len()
    );
    assert_eq!(
        body["item"]["payloadManifest"]["sha256"],
        digest(&capture_bytes)
    );
    assert_eq!(
        payload_puts[0].body, capture_bytes,
        "the pointer names the exact actual payload upload"
    );
    let capture: Value =
        serde_json::from_slice(&capture_bytes).expect("actual uploaded capture JSON");
    assert_eq!(capture["directory"], "/project");
    let item = &capture["item"];
    assert_eq!(item["content"], text);
    assert_eq!(item["text"], text);
    assert_eq!(
        item["sendConfig"],
        json!({"providerID":"provider","modelID":"model","agent":"agent","variant":"effort"})
    );
    assert_eq!(item["attachmentManifest"][0]["filename"], "notes.txt");
    assert_eq!(item["attachmentManifest"][0]["agentPath"], model_name);
    let mut manifest_file = item["attachmentManifest"][0].clone();
    manifest_file
        .as_object_mut()
        .expect("manifest use")
        .remove("agentPath");
    assert_eq!(&manifest_file, uploaded_file);
    assert_eq!(
        item["promptParts"]["metadata"],
        json!({"pathProof":{"literal":true}})
    );
    let contexts = item["promptParts"]["context"]
        .as_array()
        .expect("captured contexts");
    assert_eq!(contexts.len(), 2);
    assert_eq!(contexts[0]["text"], "read this instruction first");
    assert_eq!(contexts[1]["text"], "captured selected context");
    assert_eq!(contexts[1]["description"], "original selection");
    // Parse the expected metadata with the same JavaScript-number semantics.
    // The observed capture and its exact bytes remain unchanged.
    let mut expected_capture = capture.clone();
    expected_capture["item"]["promptParts"]["context"][1]["metadata"] =
        json!({"source":"selection","keep":[1,true,"λ"]});
    let expected_capture = QueuePayloadDocument::from_bytes(
        serde_json::to_vec(&expected_capture).expect("expected metadata fixture JSON"),
        "/project",
        QueuePayloadUse::Enqueue,
        sha256,
    )
    .expect("expected metadata uses the supported transport parser");
    let QueuePayloadItem::Manifest(expected_item) = expected_capture.item() else {
        panic!("expected manifest metadata");
    };
    assert_eq!(
        contexts[1]["metadata"],
        serde_json::to_value(&expected_item.prompt_parts.context[1].metadata)
            .expect("normalized expected context metadata")
    );
    let document = QueuePayloadDocument::from_bytes(
        capture_bytes.clone(),
        "/project",
        QueuePayloadUse::Enqueue,
        sha256,
    )
    .expect("the actual uploaded payload passes strict native boundary parsing");
    assert!(
        document.files_proof(sha256).is_err(),
        "fresh enqueue parsing cannot invent a sealed Take proof"
    );
    assert!(
        state.items.is_empty() && state.enqueue_requests.is_empty() && state.captures.is_empty(),
        "the scripted failure creates no acceptance or admission"
    );
    drop(state);
    let journal = post
        .journal
        .as_ref()
        .expect("durable write-ahead snapshot before POST");
    let before = journal["queue"]["operations"]
        .as_array()
        .expect("journaled operations")
        .iter()
        .find(|operation| operation["id"] == operation_id)
        .expect("original operation is durable before POST")
        .clone();
    assert!(before["action"]["enqueue"]["phase"]["unknown"].is_object());
    assert!(before["action"]["enqueue"]["source"]["companion"].is_string());
    assert!(
        before["source_seal"].is_object() && before["preparation"]["inputs_object"].is_string()
    );
    assert_eq!(before["native_roots"], json!([file_id.as_str()]));
    assert_eq!(before["prepared_roots"], json!([file_id.as_str()]));
    let captured_request = post.body.clone();
    let operation_id = operation_id.to_owned();
    drop(requests);
    let (retained, source, payload, inputs, objects) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let operation = workspace
            .queue_storage
            .operations()
            .iter()
            .find(|operation| operation.id().as_str() == operation_id)
            .expect("the refused enqueue retains its exact original owner");
        assert_eq!(
            serde_json::to_value(operation).expect("retained operation"),
            before,
            "refusal cannot rewrite identity, source seal, request or roots"
        );
        let QueueAction::Enqueue {
            source: QueueSource::Companion(source),
            phase: QueueEnqueuePhase::Unknown { payload, request },
        } = operation.action()
        else {
            panic!("the original operation stays Unknown without an accepted receipt");
        };
        assert_eq!(request.bytes(), captured_request);
        assert_eq!(
            workspace
                .attachments
                .get(&file_id)
                .expect("retained local attachment")
                .blob,
            descriptor
        );
        assert_eq!(
            workspace.drafts.get(&sibling),
            Some(&sibling_draft),
            "the independent draft survives unchanged"
        );
        workspace
            .validate_queue_candidate(&workspace.queue_storage)
            .expect("all retained roots still satisfy canonical storage validation");
        let inputs = operation
            .composer_preparation()
            .and_then(|preparation| preparation.inputs_object())
            .expect("original sealed inputs")
            .clone();
        let objects = workspace
            .queue_storage
            .objects()
            .iter()
            .filter(|object| object.operation() == operation.id())
            .cloned()
            .collect::<Vec<_>>();
        for id in [source, payload, &inputs] {
            assert!(
                objects.iter().any(|object| object.id() == id),
                "each original source, payload and sealed input remains rooted"
            );
        }
        (
            operation.clone(),
            source.clone(),
            payload.clone(),
            inputs,
            objects,
        )
    });
    let spool = harness
        ._fixture
        .journal()
        .parent()
        .expect("profile")
        .join("attachment-spool");
    assert_eq!(
        fs::read(spool.join(format!("blob-{}.bin", descriptor.id().as_str())))
            .expect("original prepared bytes stay rooted"),
        b"original"
    );
    for object in objects {
        assert!(
            object.verified(),
            "every retained capture keeps its original verified chunks"
        );
        let mut bytes = Vec::new();
        for chunk in object.reference().chunks() {
            let descriptor = chunk.descriptor().expect("committed original chunk");
            let part = fs::read(spool.join(format!("blob-{}.bin", descriptor.id().as_str())))
                .expect("retained private capture chunk");
            assert_eq!(part.len() as u64, descriptor.byte_len());
            assert_eq!(sha256(&part), *descriptor.sha256());
            bytes.extend_from_slice(&part);
        }
        assert_eq!(bytes.len() as u64, object.reference().total_bytes());
        assert_eq!(&sha256(&bytes), object.reference().sha256());
        if object.id() == &source {
            assert_eq!(retained.source_sha256(), Some(&sha256(&bytes)));
            let original: EnqueueSourceCapture =
                serde_json::from_slice(&bytes).expect("retained typed source");
            assert_eq!(original.composer.native.owner, owner);
            assert_eq!(original.composer.native.draft.content, original_content);
            assert_eq!(
                original
                    .intent
                    .native_context_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                contexts
                    .iter()
                    .map(|context| context["id"]
                        .as_str()
                        .expect("original context admission ID"))
                    .collect::<Vec<_>>()
            );
        } else if object.id() == &payload {
            assert_eq!(
                bytes, capture_bytes,
                "the retained payload is exactly what the actor uploaded"
            );
        } else if object.id() == &inputs {
            let prepared = PreparedComposerInput::decode(&bytes, "/project")
                .expect("original sealed prepared input");
            assert_eq!(prepared.capture.item().files[0].name, model_name);
            assert_eq!(prepared.files, [descriptor]);
            assert_eq!(Some(&prepared.source_sha256), retained.source_sha256());
        }
    }
    cx.run_until_parked();
    assert_eq!(count(&harness.server, "/attachments/file-0"), 1);
    assert_eq!(count(&harness.server, "/attachments/payload-0"), 1);
    assert_eq!(
        count(&harness.server, "/enqueue"),
        1,
        "failure never triggers an automatic retry"
    );
    assert!(harness.queued(cx).is_empty());
    assert!(
        harness
            .server
            .calls()
            .iter()
            .all(|call| !call.starts_with("DELETE ")
                && ![
                    "/prompt",
                    "/synthetic",
                    "/command",
                    "/acknowledge",
                    "/take",
                    "/retry",
                    "/items",
                    "/reserve-prefix"
                ]
                .iter()
                .any(|suffix| call
                    .split('?')
                    .next()
                    .is_some_and(|path| path.ends_with(suffix)))),
        "no admission, acknowledgement, release or fallback route is called"
    );
    if let Some(directory) = std::env::var_os("OPENCHAMBER_NATIVE_PATH_PROOF_DIR") {
        #[cfg(unix)]
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let directory = PathBuf::from(directory);
        assert!(
            directory.is_absolute(),
            "the proof directory must be explicitly absolute"
        );
        let metadata =
            fs::metadata(&directory).expect("the provided proof directory already exists");
        assert!(
            metadata.is_dir(),
            "the proof destination is an existing directory"
        );
        #[cfg(unix)]
        assert_eq!(
            metadata.permissions().mode() & 0o077,
            0,
            "the proof directory must be private"
        );
        for (name, bytes) in [
            ("payload.json", capture_bytes.as_slice()),
            ("enqueue.json", captured_request.as_slice()),
        ] {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(directory.join(name))
                .expect("proof export cannot overwrite a file");
            file.write_all(bytes)
                .expect("exact observed proof bytes are exported");
            file.sync_all().expect("proof bytes are durable");
        }
    }
}

#[gpui_kit::test]
fn steer_mode_steers_and_ctrl_enter_sends_the_queue_now(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Steer);
    harness.type_and_enter(cx, "steer this", false);
    harness.wait(cx, "ordinary steer reaches actual prompt admission", |_| {
        count(&harness.server, "/prompt") == 1
    });
    harness.idle(cx);
    assert_eq!(harness.server.body_of("/prompt")["delivery"], "steer");
    assert!(harness.queued(cx).is_empty());

    // Two queued by another device, then Ctrl+Enter.
    harness.act(cx, |workspace, _, cx| {
        workspace.queue_composer_text_for_test("one", cx)
    });
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness.act(cx, |workspace, _, cx| {
        workspace.queue_composer_text_for_test("two", cx)
    });
    wait(cx, |cx| harness.queued(cx).len() == 2);
    harness.type_and_enter(cx, "three", true);
    harness.wait(
        cx,
        "Ctrl+Enter: reserved prefix reaches actual prompt and live queue update",
        |cx| harness.queued(cx).is_empty() && harness.composer(cx).is_empty(),
    );
    harness.idle(cx);
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(
        prompt["text"], "one\n\ntwo\n\nthree",
        "queued first, then the composer"
    );
    assert!(
        prompt.get("delivery").is_none(),
        "Ctrl+Enter leaves delivery to OpenCode"
    );
    assert!(
        harness
            .server
            .calls()
            .iter()
            .any(|call| { call == "POST /api/message-queue/sessions/ses_queue/reserve-prefix" })
    );
}

#[gpui_kit::test]
fn row_send_steers_with_the_captured_selection_and_arrow_up_edits(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "first", false);
    harness.wait(
        cx,
        "first Enter enqueue completes before second activation",
        |cx| {
            harness.queued(cx) == ["first"]
                && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
        },
    );
    harness.type_and_enter(cx, "second", false);
    harness.wait(cx, "second Enter enqueue completes", |cx| {
        harness.queued(cx).len() == 2
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let ids: Vec<QueuedItemId> = cx.read(|cx| {
        harness
            .workspace
            .read(cx)
            .selected_queue()
            .iter()
            .map(|message| message.id.clone())
            .collect()
    });
    // Reorder: second before first.
    harness.act(cx, |workspace, _, cx| {
        workspace.reorder_queued(&ids[1], &ids[0], cx)
    });
    assert_eq!(
        harness.queued(cx),
        ["second", "first"],
        "the order shows at once"
    );
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .any(|call| call.ends_with("/order"))
    });
    assert_eq!(
        harness.server.body_of("/order")["itemIds"],
        json!(["q2", "q1"])
    );
    let (enqueue, original_receipt) = {
        let state = harness
            .server
            .state
            .lock()
            .expect("original enqueue custody");
        let (request, record) = state
            .enqueue_requests
            .iter()
            .find(|(_, record)| record["receipt"]["itemId"] == ids[1].as_str())
            .expect("the selected row has its original accepted enqueue receipt");
        (
            crate::queue_capture::QueueOperationId::new(request)
                .expect("actual original enqueue ID"),
            record["receipt"].clone(),
        )
    };

    harness.act(cx, |workspace, window, cx| {
        workspace.send_queued(&ids[1], window, cx)
    });
    harness.wait(cx, "row Send completes its real Take and prompt", |cx| {
        harness.queued(cx) == ["first"] && count(&harness.server, "/prompt") == 1
    });
    harness.idle(cx);
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(prompt["text"], "second");
    assert_eq!(
        prompt["delivery"], "steer",
        "a row's send steers while busy"
    );
    assert_eq!(
        harness.server.body_of("/model")["model"]["variant"],
        "effort"
    );
    {
        let state = harness
            .server
            .state
            .lock()
            .expect("Take delivery observation");
        let original = &state.enqueue_requests[enqueue.as_str()];
        assert_eq!(
            original["state"], "taken",
            "request-scoped Take ends the original enqueue's queued delivery state"
        );
        assert_eq!(
            original["receipt"], original_receipt,
            "Take preserves the original enqueue receipt identity"
        );
        assert_eq!(
            state
                .enqueue_requests
                .values()
                .find(|record| record["receipt"]["itemId"] == ids[0].as_str())
                .expect("independent queued row")["state"],
            "queued"
        );
    }
    assert_eq!(
        count(&harness.server, "/acknowledge"),
        1,
        "the real durable Take path acknowledges its local destination"
    );
    harness.act(cx, |workspace, window, cx| {
        workspace.check_payload_enqueue(&enqueue, window, cx)
    });
    harness.wait(
        cx,
        "row Send: original enqueue status reads Taken and retires only its capture",
        |cx| {
            cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                !workspace.queue.enqueues.busy(&enqueue)
                    && workspace.queue_storage.operation(&enqueue).is_none()
            })
        },
    );
    assert!(
        harness.server.calls().iter().any(|call| {
            call.starts_with("GET ")
                && call
                    .split('?')
                    .next()
                    .is_some_and(|path| path.ends_with(&format!("/requests/{}", enqueue.as_str())))
        }),
        "terminal delivery state comes through the real enqueue lookup"
    );

    // ArrowUp in the empty composer takes the newest message back.
    let newest = cx.read(|cx| harness.workspace.read(cx).newest_editable_queued());
    assert_eq!(newest, Some(ids[0].clone()));
    harness.act(cx, |workspace, window, cx| {
        workspace.edit_queued(&ids[0], window, cx)
    });
    harness.wait(cx, "Edit materializes the actual compound editor", |cx| {
        harness.composer(cx) == "first"
            && cx.read(|cx| !harness.workspace.read(cx).queue.takes.any_busy())
    });
    assert!(
        harness.queued(cx).is_empty(),
        "editing takes it out of the queue"
    );

    // The edit took "first" out of its queue, which starts its one status
    // check; like the row's Check, it holds the session's Enter until it ends.
    harness.idle(cx);
    harness.type_and_enter(cx, "gone", false);
    wait(cx, |cx| harness.queued(cx) == ["gone"]);
    let id = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    harness.act(cx, |workspace, _, cx| workspace.remove_queued(&id, cx));
    assert!(harness.queued(cx).is_empty(), "removal shows at once");
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .any(|call| call.starts_with("DELETE"))
    });
}

#[gpui_kit::test]
fn broadcasts_apply_by_revision_and_deletion_forgets_the_queue(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    let update = |revision, contents: &[&str]| {
        let items: Vec<Value> = contents
            .iter()
            .enumerate()
            .map(|(index, content)| json!({"id": format!("b{index}"), "createdAt": 1, "content": content,
                "text": content, "attachments": [], "sendConfig": {"providerID": "p", "modelID": "m"}}))
            .collect();
        let properties = json!({"revision": revision, "session": {"sessionId": "ses_queue",
            "directory": "/project", "items": items, "sendingId": null}});
        let frame = json!({"type": "event", "payload": {"type": "openchamber:message-queue.updated", "properties": properties}});
        openchamber_native_transport::event::decode_frame(&frame.to_string())
            .expect("valid frame")
            .events
    };
    harness.act(cx, |workspace, window, cx| {
        workspace.apply_server_events(update(5, &["new"]), window, cx);
        workspace.apply_server_events(update(4, &["stale"]), window, cx);
    });
    assert_eq!(harness.queued(cx), ["new"]);
    harness.act(cx, |workspace, window, cx| {
        workspace.apply_server_events(
            vec![ServerEvent::SessionDeleted {
                session_id: harness.key.session.clone(),
            }],
            window,
            cx,
        );
        let runtime = workspace.runtime.clone();
        let key = openchamber_native_core::message_queue::QueueKey::new(
            runtime,
            "/project",
            harness.key.session.clone(),
        )
        .expect("valid key");
        assert!(
            workspace.queue.projection.queue(&key).is_empty(),
            "deletion forgets the queue"
        );
    });
}

#[gpui_kit::test]
fn escape_stops_only_on_a_second_press_within_three_seconds(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    let interrupts = |server: &QueueServer| {
        server
            .calls()
            .iter()
            .filter(|call| call.ends_with("/interrupt"))
            .count()
    };
    harness.act(cx, |workspace, window, cx| {
        workspace.focus_composer(window, cx);
    });
    cx.run_until_parked();
    harness.act(cx, |workspace, window, cx| {
        workspace.escape_to_stop(window, cx)
    });
    cx.run_until_parked();
    assert_eq!(interrupts(&harness.server), 0, "one Escape only arms");
    // An expired arm re-arms instead of stopping.
    harness.act(cx, |workspace, window, cx| {
        if let Some((_, until)) = workspace.abort_primed.as_mut() {
            *until = Instant::now() - Duration::from_millis(1);
        }
        workspace.escape_to_stop(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        interrupts(&harness.server),
        0,
        "a press after three seconds arms again"
    );
    harness.act(cx, |workspace, window, cx| {
        workspace.escape_to_stop(window, cx)
    });
    wait(cx, |_| interrupts(&harness.server) == 1);
    // Escape away from the composer never arms.
    harness.act(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle.clone(), cx);
    });
    cx.run_until_parked();
    harness.act(cx, |workspace, window, cx| {
        workspace.escape_to_stop(window, cx);
        assert!(workspace.abort_primed.is_none());
    });
}

#[gpui_kit::test]
fn starter_edits_save_the_global_list_with_both_markers(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, _, cx| {
        workspace.remove_starter("global:command:explore", cx);
    });
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .any(|call| call.starts_with("PUT /api/config/settings"))
    });
    let body = harness
        .server
        .body_of("/api/config/settings?surface=desktop");
    let names: Vec<&str> = body["draftStarters"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|starter| starter["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names.len(), 7);
    assert!(!names.contains(&"explore"));
    assert_eq!(body["draftStartersCraftGoalAdded"], true);
    assert_eq!(body["draftStartersScheduleTaskAdded"], true);
    harness.act(cx, |workspace, _, cx| {
        assert!(
            workspace
                .pinnable_starters()
                .contains(&StarterRef::command("explore"))
        );
        workspace.add_starter(StarterRef::command("explore"), cx);
        assert!(workspace.pinnable_starters().is_empty());
    });
}

impl Workspace {
    /// Queues `text` as another device would, without touching the composer.
    fn queue_composer_text_for_test(&mut self, text: &str, cx: &mut gpui_kit::Context<Self>) {
        let key = self.selected_queue_key().expect("a selected queue");
        let connection = self.connection.as_ref().expect("connected");
        let client = connection.client.clone().expect("client");
        let item = openchamber_native_transport::message_queue::QueueItemInput {
            content: text.to_owned(),
            text: text.to_owned(),
            send_config: openchamber_native_core::message_queue::QueuedSendConfig {
                provider_id: "provider".to_owned(),
                model_id: "model".to_owned(),
                agent: None,
                variant: None,
            },
        };
        let request = connection.network.run(async move {
            client
                .enqueue_message(&key.session, &key.directory, &item)
                .await
        });
        cx.spawn(async move |this, cx| {
            if let Some(Ok(update)) = request.await {
                let _ = this.update(cx, |this, cx| {
                    this.apply_queue_broadcast(update, None);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// A broadcast queue of `items` at `revision`, as another device leaves it.
fn broadcast(revision: u64, items: Vec<Value>) -> Vec<ServerEvent> {
    let properties = json!({"revision": revision, "session": {"sessionId": "ses_queue",
        "directory": "/project", "items": items, "sendingId": null}});
    let frame = json!({"type": "event", "payload": {"type": "openchamber:message-queue.updated", "properties": properties}});
    openchamber_native_transport::event::decode_frame(&frame.to_string())
        .expect("valid frame")
        .events
}

fn item(id: &str, extra: Value) -> Value {
    let mut item = json!({"id": id, "createdAt": 1, "content": id, "text": id, "attachments": [],
        "sendConfig": {"providerID": "provider", "modelID": "model"}});
    for (key, value) in extra.as_object().expect("an object") {
        item[key] = value.clone();
    }
    item
}

fn count(server: &QueueServer, needle: &str) -> usize {
    server
        .calls()
        .iter()
        .filter(|call| call.contains(needle))
        .count()
}

fn branching_command_leaves_the_queue(
    text: &str,
    secondary: bool,
    cx: &mut TestAppContext,
) -> String {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        workspace.apply_server_events(broadcast(1, vec![item("q1", json!({}))]), window, cx)
    });
    harness.type_and_enter(cx, text, secondary);
    cx.run_until_parked();
    assert_eq!(
        count(&harness.server, "/message-queue/sessions/ses_queue/items"),
        0,
        "nothing was queued"
    );
    assert_eq!(
        count(&harness.server, "/take"),
        0,
        "a branching command never takes the queue"
    );
    assert_eq!(harness.queued(cx), ["q1"]);
    assert_eq!(
        count(&harness.server, "/enqueue"),
        0,
        "branch commands never enter payload enqueue"
    );
    harness.composer(cx)
}

#[gpui_kit::test]
fn fork_on_enter_is_never_queued(cx: &mut TestAppContext) {
    let left = branching_command_leaves_the_queue("/fork", false, cx);
    assert_eq!(left, "", "the fork took the text");
}

#[gpui_kit::test]
fn host_switch_keeps_a_committed_queue_take_alive_until_its_text_is_restored(
    cx: &mut TestAppContext,
) {
    use crate::queue_storage::{QueueAction, QueueTakePhase};

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "retained from old host", false);
    harness.wait(cx, "host switch: original enqueue completes", |cx| {
        harness.queued(cx).len() == 1
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let owner = cx
        .read(|cx| harness.workspace.read(cx).composer_key.clone())
        .expect("draft owner");
    let id = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    harness.server.state.lock().expect("state").hold = Some("/take");
    harness.act(cx, |workspace, window, cx| {
        workspace.edit_queued(&id, window, cx)
    });
    harness.wait(
        cx,
        "host switch: real request-scoped Take commits on original server",
        |_| harness.server.state.lock().expect("state").items.is_empty(),
    );
    harness.act(cx, |workspace, window, cx| {
        workspace.connect_to_host(
            Endpoint::parse("http://127.0.0.1:1/").expect("endpoint"),
            window,
            cx,
        );
        workspace.switch_runtime(
            openchamber_native_core::ids::RuntimeId::from("new-host"),
            window,
            cx,
        );
        workspace.release_retired_networks();
        assert_eq!(
            workspace.retired_networks.len(),
            1,
            "the committed take still needs its response"
        );
    });
    harness.server.state.lock().expect("state").hold = None;
    harness.wait(
        cx,
        "host switch: original owner receives the durable compound destination",
        |cx| {
            cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                !workspace.queue.takes.any_busy()
                    && workspace.composer_has_companion(&owner)
                    && workspace
                        .queue_storage
                        .operations()
                        .iter()
                        .any(|operation| {
                            operation
                                .owner()
                                .draft_key()
                                .is_ok_and(|captured| captured == owner)
                                && matches!(
                                    operation.action(),
                                    QueueAction::Take {
                                        phase: QueueTakePhase::Acknowledged { .. },
                                        ..
                                    }
                                )
                        })
            })
        },
    );
    let materialized = Arc::new(Mutex::new(None::<(String, Vec<Vec<u8>>)>));
    let observed = Arc::clone(&materialized);
    harness.act(cx, |workspace, window, cx| {
        let capture = workspace.capture_draft_companions(
            &owner,
            workspace.drafts.get(&owner).cloned().unwrap_or_default(),
        );
        assert_eq!(capture.native.owner.runtime(), &harness.key.runtime);
        cx.spawn_in(window, async move |this, cx| {
            let items = crate::draft_companion::materialize(&this, cx, &capture)
                .await
                .expect("real immutable old-owner companions");
            for item in &items {
                assert_eq!(item.document.directory(), "/project");
                assert_eq!(item.document.item().send_config().provider_id, "provider");
                assert_eq!(item.document.item().send_config().model_id, "model");
                assert_eq!(
                    item.document.item().send_config().variant.as_deref(),
                    Some("effort")
                );
                assert_eq!(
                    item.document.item().send_config().agent.as_deref(),
                    Some("agent")
                );
            }
            let text = crate::draft_companion::display_text(&capture.native.draft, &items);
            *observed.lock().expect("compound display observation") = Some((
                text,
                items
                    .iter()
                    .map(|item| item.document.bytes().to_vec())
                    .collect(),
            ));
        })
        .detach();
    });
    harness.wait(
        cx,
        "host switch: actual compound editor sources materialize locally",
        |_| materialized.lock().expect("observation").is_some(),
    );
    let (text, payloads) = materialized
        .lock()
        .expect("observation")
        .take()
        .expect("complete materialization");
    assert_eq!(text, "retained from old host");
    let state = harness
        .server
        .state
        .lock()
        .expect("original server custody");
    assert_eq!(payloads, vec![state.payloads[id.as_str()].bytes.clone()]);
    drop(state);
    assert_eq!(count(&harness.server, "/take"), 1);
    assert_eq!(count(&harness.server, "/acknowledge"), 1);
    assert_ne!(
        harness.composer(cx),
        "retained from old host",
        "old-host content stays out of the new host's composer"
    );
}

#[gpui_kit::test]
fn btw_on_ctrl_enter_never_takes_the_queue(cx: &mut TestAppContext) {
    // The scripted server cannot fork, so the side conversation's own
    // failure path may give the question back; it is never queued.
    let _ = branching_command_leaves_the_queue("/btw what now", true, cx);
}

#[gpui_kit::test]
fn steer_mode_enter_takes_the_queue_first(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Steer);
    harness.act(cx, |workspace, _, cx| {
        workspace.queue_composer_text_for_test("one", cx)
    });
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness.type_and_enter(cx, "two", false);
    harness.wait(
        cx,
        "steer Enter: queued prefix is admitted and removed by the server",
        |cx| harness.queued(cx).is_empty(),
    );
    harness.idle(cx);
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(prompt["text"], "one\n\ntwo");
    assert_eq!(prompt["delivery"], "steer");
}

#[gpui_kit::test]
fn open_requests_neither_block_queueing_nor_get_dismissed_by_a_row_send(cx: &mut TestAppContext) {
    use openchamber_native_core::pending_requests::PendingRequest;
    use openchamber_native_core::permission::PermissionRequest;
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        super::request_tests::ask(
            workspace,
            PendingRequest::Permission(PermissionRequest {
                id: "per_q".into(),
                session_id: "ses_queue".into(),
                action: "shell".into(),
                resources: vec!["printf".into()],
                save: Vec::new(),
                source: None,
                message: None,
                metadata_display: None,
            }),
            window,
            cx,
        );
    });
    harness.type_and_enter(cx, "later", false);
    harness.wait(
        cx,
        "open requests: enqueue completes without dismissing permission",
        |cx| {
            harness.queued(cx) == ["later"]
                && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
        },
    );
    let id = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    harness.act(cx, |workspace, window, cx| {
        workspace.send_queued(&id, window, cx)
    });
    harness.wait(cx, "open requests: real row Take admits its prompt", |_| {
        count(&harness.server, "/prompt") == 1
    });
    harness.idle(cx);
    assert_eq!(
        count(&harness.server, "/reply"),
        0,
        "no request was answered"
    );
    assert_eq!(harness.server.body_of("/prompt")["text"], "later");
}

#[gpui_kit::test]
fn a_lost_reservation_reply_recovers_the_same_message_without_losing_new_typing(
    cx: &mut TestAppContext,
) {
    use crate::queue_storage::{QueueAction, QueuePrefixPhase, QueueSource};

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "A", false);
    harness.wait(cx, "lost reservation: first enqueue completes", |cx| {
        harness.queued(cx).len() == 1
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    harness.type_and_enter(cx, "B", false);
    harness.wait(cx, "lost reservation: second enqueue completes", |cx| {
        harness.queued(cx).len() == 2
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    harness
        .server
        .state
        .lock()
        .expect("state")
        .drop_reservation_reply = true;
    harness.type_and_enter(cx, "typed", true);
    harness.wait(
        cx,
        "lost reservation: original reserve POST loses only its reply",
        |cx| {
            count(&harness.server, "/reserve-prefix") == 1
                && cx.read(|cx| !harness.workspace.read(cx).queue.prefixes.any_busy())
        },
    );
    let (operation, frame, source, configuration) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let operation = workspace
            .queue_storage
            .operations()
            .iter()
            .find(|operation| matches!(operation.action(), QueueAction::Prefix { .. }))
            .expect("retained original prefix");
        let QueueAction::Prefix {
            source: QueueSource::Companion(source),
            phase: QueuePrefixPhase::ReservationUnknown(handoff),
            ..
        } = operation.action()
        else {
            panic!("a lost reserve reply retains the immutable original handoff");
        };
        assert!(
            workspace
                .transfer_recovery_items()
                .iter()
                .any(|row| row.id == *operation.id() && row.can_check && row.can_retry)
        );
        (
            operation.id().clone(),
            handoff.prompt.clone(),
            source.clone(),
            operation.configuration().restore(),
        )
    });
    let id = harness
        .server
        .state
        .lock()
        .expect("state")
        .reservation
        .as_ref()
        .expect("server retained receipt")["id"]
        .clone();
    {
        let requests = harness.server.requests.lock().expect("recorded requests");
        let reservation = requests
            .iter()
            .find(|request| request.path.ends_with("/reserve-prefix"))
            .expect("reservation request reached the server");
        let stored = reservation
            .journal
            .as_ref()
            .expect("journal exists before reservation");
        let retained = stored["queue"]["operations"]
            .as_array()
            .expect("durable operation ledger")
            .iter()
            .find(|saved| saved["id"] == operation.as_str())
            .expect("original prefix precedes reservation");
        assert_eq!(retained["action"]["prefix"]["message"], id);
        assert!(retained["action"]["prefix"]["phase"]["reservation_unknown"].is_object());
        assert!(retained["source_seal"].is_object());
    }
    let original_source: crate::queue_storage::PrefixSourceCapture =
        serde_json::from_slice(&harness.object_bytes(cx, &source))
            .expect("original native source remains sealed");
    assert_eq!(original_source.composer.native.draft.text, "typed");
    let frame_bytes = harness.object_bytes(cx, &frame);
    let client = cx
        .read(|cx| {
            harness
                .workspace
                .read(cx)
                .connection
                .as_ref()
                .expect("original host")
                .client
                .clone()
        })
        .expect("captured client");
    let message = MessageId::from(id.as_str().expect("original prompt ID").to_owned());
    let frozen = openchamber_native_transport::queue_frozen_send::QueueFrozenSend::from_bytes(
        &client,
        openchamber_native_transport::queue_frozen_send::QueueFrozenSendOwner {
            session: &harness.key.session,
            directory: "/project",
            message_id: &message,
            configuration: &configuration,
        },
        frame_bytes.clone(),
        crate::queue_capture::sha256(&frame_bytes),
        crate::queue_capture::sha256,
    )
    .expect("the durable original frame reconstructs before recovery");
    assert_eq!(
        serde_json::from_slice::<Value>(frozen.prompt_body()).expect("actual frozen prompt")["text"],
        "A\n\nB\n\ntyped"
    );
    harness.act(cx, |workspace, window, cx| {
        workspace.check_payload_prefix(&operation, window, cx)
    });
    harness.wait(
        cx,
        "lost reservation: explicit Check observes Missing without replay",
        |cx| {
            cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                !workspace.queue.prefixes.busy(&operation)
                    && workspace.queue.prefixes.check(&operation)
                        == crate::queue_prefix::PrefixCheck::Missing
            })
        },
    );
    assert_eq!(count(&harness.server, "/reserve-prefix"), 1);
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert_eq!(harness.composer(cx), "");
    assert_eq!(harness.server.state.lock().expect("state").items.len(), 2);
    let queued_before = count(&harness.server, "/enqueue");
    harness.type_and_enter(cx, "newer", false);
    assert_eq!(
        count(&harness.server, "/enqueue"),
        queued_before,
        "an uncertain reservation cannot queue the captured text a second time"
    );
    close(cx, harness.window, &harness.workspace);
    let (reopened_window, reopened) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &reopened);
    cx.update_window(reopened_window, |_, window, cx| {
        reopened.update(cx, |workspace, cx| {
            configure_queue_session(workspace, &harness.key, window, cx);
            assert_eq!(workspace.composer.read(cx).value().as_ref(), "newer");
            assert!(
                workspace.payload_prefix_blocks_session(&harness.key.runtime, &harness.key.session)
            );
        });
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-prefix-{}-retry", operation.as_str()).into()),
            cx,
        );
    })
    .expect("reopened window");
    wait_queue(
        cx,
        reopened_window,
        &reopened,
        &harness.server,
        "lost reservation: explicit original-ID Retry admits once",
        |cx| {
            count(&harness.server, "/prompt") == 1
                && cx.read(|cx| !reopened.read(cx).queue.prefixes.any_busy())
        },
    );
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(prompt["id"], id);
    assert_eq!(prompt["text"], "A\n\nB\n\ntyped");
    assert_eq!(
        cx.read(|cx| reopened.read(cx).composer.read(cx).value().to_string()),
        "newer"
    );
    assert_eq!(
        count(&harness.server, "/take"),
        0,
        "no destructive take is used"
    );
    close(cx, reopened_window, &reopened);
}

#[gpui_kit::test]
fn an_empty_take_without_text_sends_nothing(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        workspace.live_status.remove(&harness.key.session);
        // Shown here, already delivered on the server.
        workspace.apply_server_events(broadcast(1, vec![item("gone", json!({}))]), window, cx)
    });
    harness.type_and_enter(cx, "", false);
    wait(cx, |_| count(&harness.server, "/preview-prefix") == 1);
    cx.run_until_parked();
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert!(
        cx.read(|cx| harness.workspace.read(cx).persistence_ready()),
        "the journal never saw an empty prompt"
    );
}

#[gpui_kit::test]
fn a_stale_preview_restores_only_the_captured_composer_and_releases_the_session(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "already delivered", false);
    harness.wait(cx, "stale preview: enqueue completes", |cx| {
        harness.queued(cx).len() == 1
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    harness
        .server
        .state
        .lock()
        .expect("queue state")
        .invalidate_after_preview = true;
    harness.type_and_enter(cx, "still unsent", true);
    harness.wait(
        cx,
        "stale preview: refused reserve restores input and releases the prefix",
        |cx| {
            count(&harness.server, "/reserve-prefix") == 1
                && cx.read(|cx| {
                    !harness
                        .workspace
                        .read(cx)
                        .queue_preview_pending(&harness.key.session)
                })
        },
    );
    assert_eq!(harness.composer(cx), "still unsent");
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert!(
        harness
            .server
            .state
            .lock()
            .expect("queue state")
            .reservation
            .is_none()
    );
    harness.type_and_enter(cx, "still unsent", false);
    harness.wait(
        cx,
        "stale preview: restored input can enqueue independently",
        |cx| harness.queued(cx) == ["still unsent"],
    );
}

#[gpui_kit::test]
fn queue_intent_budget_rejection_leaves_originals_unreserved_and_storage_usable(
    cx: &mut TestAppContext,
) {
    use crate::mutation_journal::JournalError;
    use crate::queue_capture::{QueueJson, QueueOperationId};
    use crate::queue_storage::{
        NativePrefixIntent, NativePrefixKind, QueueConfiguration, QueueOperation, QueueOwner,
    };

    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "queued budget original", false);
    harness.wait(cx, "journal budget: original enqueue completes", |cx| {
        harness.queued(cx).len() == 1
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    let text = "captured".repeat(512);
    let padding_owner = tests::attempt(&tests::key("queue-runtime", "ses_budget_padding")).owner;
    harness.act(cx, |workspace, window, cx| {
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value(text.clone(), window, cx)
        });
        workspace.save_composer_draft(cx);
        for index in 0..15 {
            let owner =
                tests::attempt(&tests::key("queue-runtime", &format!("ses_budget_{index}"))).owner;
            workspace.drafts.save(
                owner,
                Draft {
                    text: "d".repeat(1024 * 1024),
                    ..Draft::default()
                },
            );
        }
        assert!(
            workspace.validate_queue_intent_admission().is_ok(),
            "other retained drafts fit by themselves"
        );
        // Find the real canonical journal boundary with bounded actual draft
        // content. Queued context belongs in the spool and is not journal fill.
        let (mut low, mut high) = (0_usize, 1024 * 1024);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            workspace
                .drafts
                .save(padding_owner.clone(), Draft::new("p".repeat(middle)));
            match workspace.validate_queue_intent_admission() {
                Ok(()) => low = middle,
                Err(JournalError::Invalid("encoded file size limit exceeded")) => high = middle - 1,
                Err(error) => {
                    panic!("journal budget fixture failed a different invariant: {error}")
                }
            }
        }
        assert!(low > text.len());
        workspace.drafts.save(
            padding_owner.clone(),
            Draft::new("p".repeat(low - text.len())),
        );
        workspace
            .validate_queue_intent_admission()
            .expect("every current owner remains inside the 16 MiB journal limit");
        let capture = workspace
            .capture_composer_submission(cx)
            .expect("real current native input");
        let message = workspace.message_ids.next().0;
        let id = QueueOperationId::new(workspace.message_ids.next().0.as_str())
            .expect("real candidate identity");
        let owner = &capture.composer.native.owner;
        let intent = NativePrefixIntent {
            kind: NativePrefixKind::Capture,
            directory: "/project".into(),
            id: message.to_string(),
            item_ids: vec![workspace.selected_queue()[0].id.to_string()],
            delivery: None,
            companions: capture.composer.references.clone(),
            native_context_ids: Vec::new(),
            files: Vec::new(),
            edit: capture.edit.clone(),
        };
        let (_, selection) = workspace
            .send_selection()
            .expect("authoritative current configuration");
        let model = selection.model.expect("captured model");
        let configuration = openchamber_native_core::message_queue::QueuedSendConfig {
            provider_id: model.provider_id,
            model_id: model.model_id,
            agent: selection.agent_name,
            variant: model.variant,
        };
        let mut candidate = QueueOperation::prefix(
            id,
            QueueOwner::new(owner.runtime(), "/project", &harness.key.session, owner)
                .expect("exact original candidate owner"),
            QueueConfiguration::capture(&configuration),
            message,
            capture.composer.native.clone(),
            QueueJson::new(serde_json::to_vec(&intent).expect("real candidate intent"))
                .expect("bounded intent"),
        )
        .expect("valid complete candidate source");
        candidate
            .begin_composer_preparation(capture.generation, capture.revision)
            .expect("normal new-protocol marker");
        let mut queue = workspace.queue_storage.clone();
        let (error, _) = queue
            .admit_composer_operation(candidate, Vec::new(), |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
            .expect_err(
                "adding the actual native source must exceed the canonical encoded journal budget",
            );
        assert!(matches!(
            error,
            JournalError::Invalid("encoded file size limit exceeded")
        ));
    });
    harness.type_and_enter(cx, &text, true);
    harness.wait(
        cx,
        "journal budget: canonical admission refuses without reserving",
        |cx| {
            cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                !workspace.queue_preview_pending(&harness.key.session)
                    && !workspace.send_attempts.is_active(&harness.key)
            })
        },
    );
    assert_eq!(
        count(&harness.server, "/preview-prefix"),
        0,
        "full-snapshot admission precedes preview"
    );
    assert_eq!(
        count(&harness.server, "/reserve-prefix"),
        0,
        "local admission precedes ownership"
    );
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert_eq!(harness.composer(cx), text);
    assert!(
        harness
            .server
            .state
            .lock()
            .expect("state")
            .reservation
            .is_none()
    );
    assert_eq!(harness.server.state.lock().expect("state").items.len(), 1);
    harness.act(cx, |workspace, _, _| {
        assert!(
            workspace.persistence_ready(),
            "unsupported merge cannot fault all local storage"
        );
        assert!(
            workspace.send_attempts.can_send(&harness.key),
            "no unsupported intent remains"
        );
        for index in 0..15 {
            let owner =
                tests::attempt(&tests::key("queue-runtime", &format!("ses_budget_{index}"))).owner;
            workspace.drafts.take(&owner);
        }
        workspace.drafts.take(&padding_owner);
    });
}

#[gpui_kit::test]
fn queue_intent_write_failure_never_reserves_and_releases_the_local_capture(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "queued", false);
    harness.wait(
        cx,
        "write refusal: real enqueue completes before the next activation",
        |cx| {
            harness.queued(cx).len() == 1
                && harness._fixture.journal().is_file()
                && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
        },
    );
    let backup = harness._fixture.0.join("queue-write-backup.json");
    fs::rename(harness._fixture.journal(), &backup).expect("retain journal");
    fs::create_dir(harness._fixture.journal()).expect("force atomic rename failure");
    harness.type_and_enter(cx, "captured", true);
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = harness.workspace.read(cx);
            !workspace.persistence_ready() && !workspace.send_attempts.is_active(&harness.key)
        })
    });
    assert_eq!(count(&harness.server, "/reserve-prefix"), 0);
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert_eq!(harness.composer(cx), "captured");
    assert!(
        harness
            .server
            .state
            .lock()
            .expect("state")
            .reservation
            .is_none()
    );
    assert_eq!(harness.server.state.lock().expect("state").items.len(), 1);
    assert!(cx.read(|cx| {
        harness
            .workspace
            .read(cx)
            .send_attempts
            .can_send(&harness.key)
    }));
    fs::remove_dir(harness._fixture.journal()).expect("remove test obstacle");
    fs::rename(backup, harness._fixture.journal()).expect("restore valid journal");
    harness.act(cx, |workspace, window, cx| {
        workspace.retry_persistence(window, cx)
    });
    wait(cx, |cx| {
        cx.read(|cx| harness.workspace.read(cx).persistence_ready())
    });
}

#[gpui_kit::test]
fn retry_of_a_row_send_keeps_open_requests_and_its_prompt_identity(cx: &mut TestAppContext) {
    use crate::queue_storage::{QueueAction, QueueTakePurpose};
    use openchamber_native_core::pending_requests::PendingRequest;
    use openchamber_native_core::permission::PermissionRequest;
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "queued", false);
    harness.wait(cx, "row Retry: enqueue completes", |cx| {
        harness.queued(cx).len() == 1
            && cx.read(|cx| !harness.workspace.read(cx).queue.enqueues.any_busy())
    });
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("prompt");
    harness.act(cx, |workspace, window, cx| {
        super::request_tests::ask(
            workspace,
            PendingRequest::Permission(PermissionRequest {
                id: "per_retry".into(),
                session_id: "ses_queue".into(),
                action: "shell".into(),
                resources: vec!["printf".into()],
                save: Vec::new(),
                source: None,
                message: None,
                metadata_display: None,
            }),
            window,
            cx,
        );
        let id = workspace.selected_queue()[0].id.clone();
        workspace.send_queued(&id, window, cx);
    });
    harness.wait(
        cx,
        "row Retry: actual frozen prompt receives refusal",
        |_| count(&harness.server, "/prompt") == 1,
    );
    harness.idle(cx);
    let original = harness.server.body_of("/prompt");
    let id = MessageId::new(original["id"].as_str().expect("message id"));
    let operation = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        assert_eq!(workspace.requests_for_send(&harness.key).len(), 1, "row Send retains its open permission");
        workspace.queue_storage.operations().iter().find(|operation| matches!(operation.action(),
            QueueAction::Take { purpose: QueueTakePurpose::Send { message, .. }, .. } if message == id.as_str()))
            .expect("the real Take owns its refused frozen send").id().clone()
    });
    harness.server.state.lock().expect("state").fail.clear();
    // The UI Retry path performs an exact-ID observation before dispatch.
    harness.act(cx, |workspace, window, cx| {
        workspace.check_payload_take(&operation, window, cx)
    });
    harness.wait(
        cx,
        "row Retry: explicit Take Check observes missing prompt",
        |cx| {
            cx.read(|cx| {
                let workspace = harness.workspace.read(cx);
                !workspace.queue.takes.busy(&operation)
                    && workspace.queue.takes.check(&operation)
                        == crate::queue_take::TakeCheck::Missing
                    && workspace
                        .transfer_recovery_items()
                        .iter()
                        .any(|row| row.id == operation && row.can_retry)
            })
        },
    );
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::ElementId::Name(format!("queue-take-{}-retry", operation.as_str()).into()),
            cx,
        );
    })
    .expect("actual Take Retry control");
    harness.wait(cx, "row Retry: identical frozen prompt is admitted", |_| {
        count(&harness.server, "/prompt") == 2
    });
    harness.idle(cx);
    assert_eq!(harness.server.body_of("/prompt")["id"], original["id"]);
    assert_eq!(
        count(&harness.server, "/take"),
        1,
        "prompt Retry never takes the source again"
    );
    assert_eq!(
        count(&harness.server, "/acknowledge"),
        1,
        "the original durable handoff is acknowledged once"
    );
    assert_eq!(
        cx.read(|cx| harness
            .workspace
            .read(cx)
            .requests_for_send(&harness.key)
            .len()),
        1
    );
    assert_eq!(
        count(&harness.server, "/reply"),
        0,
        "neither the original send nor Retry dismisses requests"
    );
}

#[gpui_kit::test]
fn goal_and_unjournalable_messages_stay_for_the_server(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    let big = json!({"attachments": [{"id": "a", "filename": "big.png", "mimeType": "image/png",
        "size": 100_000, "source": "local"}]});
    harness.act(cx, |workspace, window, cx| {
        workspace.live_status.remove(&harness.key.session);
        workspace.apply_server_events(
            broadcast(
                1,
                vec![
                    item("goal", json!({"goal": {"tokenBudget": null}})),
                    item(
                        "scheduled",
                        json!({"scheduledTask":{"projectId":"registered","taskId":"fixture-task"}}),
                    ),
                    item("big", big),
                ],
            ),
            window,
            cx,
        );
        let projected = workspace.selected_queue();
        assert!(!projected[0].payload_takeable() && !projected[1].payload_takeable());
        assert!(
            projected[2].payload_takeable(),
            "a 100 kB file is eligible for a bounded payload transfer"
        );
        assert!(
            !projected[2].takeable(64 * 1024),
            "the former inline-reference limit remains a distinct contract"
        );
        for id in ["goal", "scheduled"] {
            workspace.send_queued(&QueuedItemId::new(id), window, cx);
        }
    });
    harness.type_and_enter(cx, "", false);
    cx.run_until_parked();
    assert_eq!(
        count(&harness.server, "/take"),
        0,
        "goals and scheduled messages remain server-owned"
    );
    assert_eq!(harness.queued(cx), ["goal", "scheduled", "big"]);
    assert_eq!(
        cx.read(|cx| harness.workspace.read(cx).newest_editable_queued()),
        Some(QueuedItemId::new("big"))
    );
    assert!(
        cx.read(|cx| harness
            .workspace
            .read(cx)
            .queue_storage
            .operations()
            .is_empty()),
        "eligibility and display never manufacture a Take owner"
    );
}

#[test]
#[should_panic(expected = "the pointer retains the stored blob identity")]
fn fixture_rejects_pointer_blob_id_substitution_with_exact_labels_and_bytes() {
    let mut state = State::default();
    let bytes = serde_json::to_vec(
        &json!({"directory":"/project","item":{"format":"manifest-v1",
        "content":"owned pointer bytes","text":"owned pointer bytes","attachmentManifest":[],
        "sendConfig":{"providerID":"provider","modelID":"model"}}}),
    )
    .expect("actual fixture capture bytes");
    let payload = store_fixture_capture(&mut state, "original_pointer_capture", bytes.clone());
    assert_eq!(
        pointer_bytes(&payload.pointer, &state),
        bytes,
        "the exact original pointer reads its genuine stored bytes"
    );
    let mut chunks = payload.pointer.chunks().to_vec();
    chunks[0].attachment.blob_id[0] ^= 1;
    let forged = QueuePayloadPointer::new(
        payload.pointer.total_bytes(),
        payload.pointer.sha256(),
        chunks,
    )
    .expect("a substituted digest still has valid pointer metadata shape");
    assert_eq!(
        forged.chunks()[0].attachment.request_id,
        payload.pointer.chunks()[0].attachment.request_id
    );
    assert_eq!(
        forged.chunks()[0].attachment.attachment_id,
        payload.pointer.chunks()[0].attachment.attachment_id
    );
    assert_eq!(
        forged.chunks()[0].attachment.sha256,
        payload.pointer.chunks()[0].attachment.sha256
    );
    assert_eq!(
        forged.chunks()[0].attachment.size,
        payload.pointer.chunks()[0].attachment.size
    );
    pointer_bytes(&forged, &state);
}

#[test]
fn payload_pointer_and_retained_metadata_keep_their_separate_admission_budgets() {
    use crate::mutation_journal::JournalError;
    use crate::queue_capture::{QUEUE_BODY_BYTES, QueueJson};
    use openchamber_native_transport::queue_manifest::{
        QUEUE_PAYLOAD_BYTES, QueueManifestError, QueuePayloadChunk,
    };

    // This is root-metadata preflight, not a claim that any downloaded bytes
    // exist. Every chunk descriptor is individually within its 20 MiB ceiling.
    let chunks = [20 * 1024 * 1024, 20 * 1024 * 1024, 10 * 1024 * 1024]
        .into_iter()
        .enumerate()
        .map(|(ordinal, size)| QueuePayloadChunk {
            ordinal: ordinal as u64,
            attachment: QueueAttachmentDescriptor {
                blob_id: crate::queue_capture::sha256(format!("limit-root-{ordinal}").as_bytes()),
                request_id: "fixture_limit_pointer".into(),
                attachment_id: format!("payload-{ordinal}"),
                filename: format!("payload-{ordinal}.json"),
                mime_type: "application/json".into(),
                size,
                sha256: crate::queue_capture::sha256(b"unmaterialized pointer declaration"),
            },
        })
        .collect::<Vec<_>>();
    let digest = crate::queue_capture::sha256(b"unmaterialized whole declaration");
    QueuePayloadPointer::new(QUEUE_PAYLOAD_BYTES as u64, digest, chunks.clone())
        .expect("50 MiB is inclusive for root metadata");
    let mut too_large = chunks;
    too_large[2].attachment.size += 1;
    assert_eq!(
        QueuePayloadPointer::new(QUEUE_PAYLOAD_BYTES as u64 + 1, digest, too_large).err(),
        Some(QueueManifestError::PayloadTooLarge),
        "the strict 50 MiB root bound refuses before capture allocation or download"
    );

    let prefix = b"{\"padding\":\"";
    let suffix = b"\"}";
    let mut metadata = Vec::with_capacity(QUEUE_BODY_BYTES + 1);
    metadata.extend_from_slice(prefix);
    metadata.resize(QUEUE_BODY_BYTES - suffix.len(), b'x');
    metadata.extend_from_slice(suffix);
    let exact = QueueJson::new(metadata.clone())
        .expect("actual canonical JSON fits the inclusive 4 MiB metadata bound");
    assert_eq!(exact.bytes().len(), QUEUE_BODY_BYTES);
    metadata.insert(metadata.len() - suffix.len(), b'x');
    assert!(matches!(
        QueueJson::new(metadata),
        Err(JournalError::Invalid("queue body size limit exceeded"))
    ));
}

/// A new-session draft in the registered `/project`.
fn draft_in_project(harness: &Harness, cx: &mut TestAppContext) {
    harness.act(cx, |workspace, window, cx| {
        workspace.topology.projects = vec![openchamber_native_core::ownership::ConfiguredProject {
            id: openchamber_native_core::ids::ProjectId::from("registered"),
            path: "/project".into(),
        }];
        workspace.show_new_session(window, cx);
        workspace.choose_draft_project("/project", window, cx);
        assert!(workspace.can_create_connected());
    });
}

#[gpui_kit::test]
fn a_starter_takes_only_the_text_it_carries_and_gives_it_back_on_failure(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    draft_in_project(&harness, cx);
    let chip = "global:command:craft-goal";
    harness.server.state.lock().expect("state").hold = Some("/magic-prompts");
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("idea", window, cx));
        workspace.activate_starter(chip, window, cx);
        workspace.save_composer_draft(cx);
        let owner = workspace.composer_key.as_ref().expect("owner");
        assert_eq!(
            workspace
                .starters
                .captures
                .iter()
                .find(|capture| &capture.owner == owner)
                .expect("the capture retains the carried draft")
                .draft
                .text,
            "idea"
        );
        assert!(
            workspace.drafts.get(owner).is_none(),
            "the carried draft has exactly one owner"
        );
        assert_eq!(
            workspace.composer.read(cx).value().as_ref(),
            "",
            "the carried text leaves at once"
        );
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value("typed while loading", window, cx)
        });
        workspace.save_composer_draft(cx);
        assert_eq!(
            workspace
                .drafts
                .get(workspace.composer_key.as_ref().expect("owner"))
                .expect("newer typing has its own draft")
                .text,
            "typed while loading"
        );
        assert_eq!(
            workspace
                .starters
                .captures
                .iter()
                .next()
                .expect("original capture remains")
                .draft
                .text,
            "idea"
        );
        let _ = workspace.persistence_barrier(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|stored| {
                stored["drafts"].as_array().is_some_and(|drafts| {
                    drafts
                        .iter()
                        .any(|draft| draft["text"] == "typed while loading")
                }) && stored["starter_captures"]
                    .as_array()
                    .is_some_and(|captures| {
                        captures
                            .iter()
                            .any(|capture| capture["draft"]["text"] == "idea")
                    })
            })
    });
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |cx| {
        cx.read(|cx| harness.workspace.read(cx).creations.iter().count() == 1)
    });
    assert_eq!(
        harness.composer(cx),
        "typed while loading",
        "typing during the read stays"
    );
    let (draft, prompt) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let creation = workspace.creations.iter().next().expect("a creation").1;
        (creation.draft.text.clone(), creation.prompt.text.clone())
    });
    assert_eq!(
        draft, "/craft-goal idea",
        "a failure gives back what the chip submitted"
    );
    assert!(
        prompt.ends_with("Here is my initial idea:\nidea"),
        "the session gets the render"
    );
    assert!(cx.read(|cx| harness.workspace.read(cx).persistence_ready()));
}

#[gpui_kit::test]
fn a_windows_spelled_ready_target_materializes_its_retained_uri(cx: &mut TestAppContext) {
    use crate::attachment_catalog::{PreparedAttachment, StagingDescriptor, StagingPhase};
    use crate::attachment_spool::{BlobDescriptor, BlobId};
    use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
    use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
    use openchamber_native_core::ids::{AttachmentId, DraftId};

    let harness = harness(cx, FollowUpBehavior::Queue);
    let owner = DraftKey::new(
        harness.key.runtime.clone(),
        "C:/project",
        DraftTarget::NewSession(DraftId::from("windows-spelling")),
    )
    .expect("Windows owner");
    let id = AttachmentId::from("0123456789abcdef0123456789abcdef");
    let target = "c:\\Temp\\openchamber\\0123456789abcdef\\captured %.txt";
    let uri = crate::attachment_catalog::file_uri(target);
    harness.act(cx, |workspace, window, cx| {
        let mut draft = Draft::new("retained content");
        draft.content.files.push(DraftFile {
            id: id.clone(),
            name: Some("captured %.txt".to_owned()),
            media_type: Some("text/plain".to_owned()),
            byte_len: Some(3),
            source_document_id: None,
            content: DraftFileContent::Prepared,
        });
        workspace.attachments.insert(
            id.clone(),
            PreparedAttachment {
                blob: BlobDescriptor::new(BlobId::parse(id.as_str()).expect("blob"), 3, [7; 32]),
                staging: Some(StagingDescriptor {
                    runtime: harness.key.runtime.clone(),
                    directory: "c:\\project".to_owned(),
                    temporary_directory: "c:\\Temp".to_owned(),
                    server_path: target.to_owned(),
                    uri: uri.clone(),
                    filename: "captured %.txt".to_owned(),
                    phase: StagingPhase::Ready,
                }),
            },
        );
        workspace.composer_key = Some(owner.clone());
        workspace.drafts.save(owner.clone(), draft);
        workspace.restore_composer_draft(window, cx);
        workspace.start_background_save(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|stored| {
                stored["attachments"].as_array().is_some_and(|files| {
                    files.iter().any(|file| file["staging"]["phase"] == "ready")
                })
            })
    });
    harness.act(cx, |workspace, _, cx| {
        assert!(workspace.prepared_attachment_ready(&owner, &id));
        let draft = workspace.composer_draft(cx);
        let parts = workspace
            .capture_draft_parts(&owner, &draft)
            .expect("normalized owner matches the retained target");
        assert_eq!(parts.files[0].uri, uri);
        assert_eq!(
            workspace.attachments[&id]
                .staging
                .as_ref()
                .expect("original target")
                .server_path,
            target
        );
    });
}

#[gpui_kit::test]
fn an_ordinary_prompt_waits_without_being_consumed_while_its_starter_is_pending(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    draft_in_project(&harness, cx);
    harness.server.state.lock().expect("state").hold = Some("/magic-prompts");
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("idea", window, cx));
        workspace.activate_starter("global:command:craft-goal", window, cx);
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value("independent prompt", window, cx)
        });
        assert!(!workspace.submit_new_session_with(None, window, cx));
        assert_eq!(
            workspace.composer.read(cx).value().as_ref(),
            "independent prompt"
        );
        assert_eq!(
            workspace.creations.iter().count(),
            0,
            "a conflicting source never enters creation"
        );
        assert_eq!(
            workspace
                .starters
                .captures
                .iter()
                .next()
                .expect("starter remains")
                .draft
                .text,
            "idea"
        );
        workspace.start_background_save(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|stored| {
                stored["drafts"].as_array().is_some_and(|drafts| {
                    drafts
                        .iter()
                        .any(|draft| draft["text"] == "independent prompt")
                }) && stored["starter_captures"]
                    .as_array()
                    .is_some_and(|captures| {
                        captures
                            .iter()
                            .any(|capture| capture["draft"]["text"] == "idea")
                    })
            })
    });
    assert!(cx.read(|cx| harness.workspace.read(cx).persistence_ready()));
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |cx| {
        cx.read(|cx| harness.workspace.read(cx).creations.iter().count() == 1)
    });
    assert_eq!(
        harness.composer(cx),
        "independent prompt",
        "the starter handoff preserves newer typing"
    );
}

#[gpui_kit::test]
fn a_failed_starter_read_gives_the_carried_text_back_after_newer_typing(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    draft_in_project(&harness, cx);
    let chip = "global:command:craft-goal";
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("magic-prompts");
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("second", window, cx));
        workspace.activate_starter(chip, window, cx);
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("newer", window, cx));
    });
    wait(cx, |cx| harness.composer(cx) == "newer\n\nsecond");
}

#[gpui_kit::test]
fn a_failed_starter_read_restores_full_content_after_a_loaded_write_failure(
    cx: &mut TestAppContext,
) {
    use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
    use openchamber_native_core::ids::AttachmentId;

    let harness = harness(cx, FollowUpBehavior::Queue);
    draft_in_project(&harness, cx);
    {
        let mut state = harness.server.state.lock().expect("state");
        state.hold = Some("/magic-prompts");
        state.fail.push("magic-prompts");
    }
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("idea", window, cx));
        workspace.save_composer_draft(cx);
        let owner = workspace.composer_key.clone().expect("draft owner");
        let mut draft = workspace.drafts.take(&owner).expect("original draft");
        draft.content.files.push(DraftFile {
            id: AttachmentId::from("starter-file"),
            name: Some("original.txt".to_owned()),
            media_type: Some("text/plain".to_owned()),
            byte_len: None,
            source_document_id: None,
            content: DraftFileContent::Reference {
                uri: "file:///project/original.txt".to_owned(),
            },
        });
        workspace.drafts.save(owner.clone(), draft);
        workspace.activate_starter("global:command:craft-goal", window, cx);
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("newer", window, cx));
        let mut invalid = openchamber_native_core::draft::Draft::new("newer");
        invalid.content.metadata_json = Some("[]".to_owned());
        workspace.drafts.save(owner, invalid);
        workspace.start_background_save(cx);
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            matches!(
                harness.workspace.read(cx).persistence_notice(),
                Some(crate::recovery_view::StorageNotice::WriteRejected)
            )
        })
    });
    harness.act(cx, |workspace, window, cx| {
        assert!(workspace.persistence_loaded());
        assert!(!workspace.persistence_ready());
        let id = workspace
            .starters
            .captures
            .iter()
            .next()
            .expect("original capture")
            .id
            .clone();
        assert!(
            !workspace.restore_starter_capture(&id, window, cx),
            "an active read retains its capture"
        );
        let owner = workspace.composer_key.clone().expect("current owner");
        let mut repaired = workspace.drafts.take(&owner).expect("newer draft remains");
        repaired.content.metadata_json = None;
        workspace.drafts.save(owner, repaired);
    });
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |cx| harness.composer(cx) == "newer\n\nidea");
    harness.act(cx, |workspace, _, cx| {
        assert!(
            workspace.starters.captures.iter().next().is_none(),
            "the full capture returned once"
        );
        let draft = workspace.composer_draft(cx);
        assert_eq!(draft.content.files[0].name.as_deref(), Some("original.txt"));
        workspace.start_background_save(cx);
    });
    wait(cx, |cx| {
        cx.read(|cx| harness.workspace.read(cx).persistence_ready())
    });
    let stored: Value =
        serde_json::from_slice(&fs::read(harness._fixture.journal()).expect("repaired journal"))
            .expect("valid replacement snapshot");
    assert!(
        stored["starter_captures"]
            .as_array()
            .expect("captures")
            .is_empty()
    );
    assert!(
        stored["drafts"]
            .as_array()
            .expect("drafts")
            .iter()
            .any(|draft| draft["text"] == "newer\n\nidea"),
        "the replacement snapshot owns the restored draft"
    );
    assert_eq!(
        count(&harness.server, "/prompt"),
        0,
        "local recovery sends no prompt"
    );
}

#[gpui_kit::test]
fn project_starters_retry_a_failed_read_and_keep_edits_made_during_a_save(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    // The selected-session Project section owns its own setup read. Settle it
    // before assigning a failure to the independent starter owner.
    harness.act(cx, |workspace, _, cx| workspace.ensure_project_readouts(cx));
    harness.wait(
        cx,
        "starter retry: Project section completes its distinct setup request",
        |_| {
            harness
                .server
                .state
                .lock()
                .expect("state")
                .completed_project_reads
                >= 1
        },
    );
    let baseline = harness.server.state.lock().expect("state").project_reads;
    {
        let mut state = harness.server.state.lock().expect("state");
        state.project_failures = 1;
        state.hold = Some("/config");
        state.project_starters = vec![
            json!({"type": "command", "name": "explore", "source": "personal"}),
            json!({"type": "command", "name": "debug", "source": "personal"}),
        ];
    }
    draft_in_project(&harness, cx);
    let reads = |server: &QueueServer| {
        server
            .calls()
            .iter()
            .filter(|call| call.starts_with("GET /api/projects/"))
            .count()
    };
    harness.act(cx, |workspace, _, cx| workspace.ensure_project_starters(cx));
    harness.wait(
        cx,
        "starter retry: held starter read consumes exactly its own failure",
        |_| {
            let state = harness.server.state.lock().expect("state");
            state.project_reads == baseline + 1 && state.project_failed_reads == 1
        },
    );
    harness.act(cx, |workspace, _, cx| workspace.ensure_project_starters(cx));
    cx.run_until_parked();
    assert_eq!(
        reads(&harness.server),
        baseline + 1,
        "an outstanding starter read is never duplicated"
    );
    let released = Instant::now();
    harness.server.state.lock().expect("state").hold = None;
    let project = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            harness
                .workspace
                .read(cx)
                .resolved_starters()
                .into_iter()
                .filter(|chip| chip.id.starts_with("project:"))
                .map(|chip| chip.starter.name)
                .collect::<Vec<_>>()
        })
    };
    harness.wait(
        cx,
        "starter retry: failed-read backoff permits one successful authoritative retry",
        |cx| {
            // Drive the same idempotent render hook while its real retry deadline
            // is pending. The bounded wait does not sleep past or extend the policy.
            harness.act(cx, |workspace, _, cx| workspace.ensure_project_starters(cx));
            project(cx) == ["explore", "debug"]
        },
    );
    assert!(
        released.elapsed() >= Duration::from_secs(2),
        "the failed starter read obeys its first retry delay"
    );
    assert_eq!(reads(&harness.server), baseline + 2);

    harness.server.state.lock().expect("state").hold = Some("/config");
    harness
        .server
        .state
        .lock()
        .expect("state")
        .project_save_failures = 1;
    harness.act(cx, |workspace, _, cx| {
        workspace.remove_starter("project:command:explore", cx);
        workspace.remove_starter("project:command:debug", cx);
    });
    assert!(
        project(cx).is_empty(),
        "both removals show while the first save is out"
    );
    harness.server.state.lock().expect("state").hold = None;
    harness.wait(
        cx,
        "starter save: failed first PUT preserves and saves the newer edit",
        |_| {
            harness
                .server
                .calls()
                .iter()
                .filter(|call| call.starts_with("PUT /api/projects/"))
                .count()
                == 2
        },
    );
    assert_eq!(
        harness.server.body_of("/config")["draftStarters"],
        json!([])
    );
    harness.wait(
        cx,
        "starter save: authoritative result retains both removals",
        |cx| project(cx).is_empty(),
    );
}
