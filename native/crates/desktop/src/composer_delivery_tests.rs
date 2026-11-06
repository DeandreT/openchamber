use super::*;

use std::sync::Mutex;
use std::time::Duration;

use openchamber_native_core::draft::{Draft, DraftTarget};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::queue_frozen_send::{
    QueueFrozenSend, QueueFrozenSendOwner, QueueFrozenSource, QueueLocalFrozenItem,
    QueueLocalSendInput,
};
use serde_json::json;

fn configuration() -> QueuedSendConfig {
    QueuedSendConfig {
        provider_id: "provider".into(),
        model_id: "selected-model".into(),
        agent: Some("build".into()),
        variant: None,
    }
}

fn local(text: &str, context: Option<&str>, metadata: serde_json::Value) -> QueueLocalCapture {
    let ids = context.into_iter().map(MessageId::from).collect::<Vec<_>>();
    QueueLocalCapture::new(
        "/project",
        QueueLocalItem {
            content: format!("display {text}"),
            text: text.into(),
            files: Vec::new(),
            send_config: configuration(),
            prompt_parts: QueuePromptParts {
                metadata: Some(Metadata(
                    serde_json::from_value(metadata).expect("typed source metadata"),
                )),
                context: ids
                    .iter()
                    .map(|id| QueuePromptContext {
                        id: Some(id.clone()),
                        text: format!("context {id}"),
                        description: None,
                        metadata: None,
                    })
                    .collect(),
                ..QueuePromptParts::default()
            },
        },
        ids,
        sha256,
    )
    .expect("genuine typed local capture")
}

#[test]
fn exact_projection_preserves_separator_edits_original_delivered_text_and_independent_metadata() {
    let native = local(
        "native delivered",
        Some("msg_native"),
        json!({"native":true}),
    );
    let original = local(
        "original delivered",
        Some("msg_original"),
        json!({"original":true}),
    );
    let captures = [native, original];
    let projected = project(
        captures.iter(),
        &[
            ProjectionPiece::Literal("[".into()),
            ProjectionPiece::Source(1),
            ProjectionPiece::Literal(" | λ | ".into()),
            ProjectionPiece::Source(0),
            ProjectionPiece::Literal("]\n".into()),
        ],
    )
    .expect("exact Source/Literal projection");
    assert_eq!(
        projected[0].item().text,
        "[original delivered | λ | native delivered]\n"
    );
    assert!(projected[1].item().text.is_empty());
    assert_eq!(projected[1].item().content, captures[1].item().content);
    assert!(
        projected[0]
            .item()
            .prompt_parts
            .metadata
            .as_ref()
            .expect("native metadata")
            .0
            .contains_key("native")
    );
    assert!(
        projected[1]
            .item()
            .prompt_parts
            .metadata
            .as_ref()
            .expect("independent original metadata")
            .0
            .contains_key("original")
    );
    assert_eq!(
        projected
            .iter()
            .flat_map(|capture| capture.context_ids())
            .cloned()
            .collect::<Vec<_>>(),
        [
            MessageId::from("msg_native"),
            MessageId::from("msg_original")
        ]
    );
}

#[test]
fn conflicting_metadata_refuses_without_mutating_either_source() {
    let captures = [
        local("first", None, json!({"same":{"value":1}})),
        local("second", None, json!({"same":{"value":2}})),
    ];
    let before = captures
        .iter()
        .map(|capture| capture.bytes().to_vec())
        .collect::<Vec<_>>();
    assert!(validate_metadata(captures.iter()).is_err());
    assert_eq!(
        before,
        captures
            .iter()
            .map(|capture| capture.bytes().to_vec())
            .collect::<Vec<_>>()
    );
    let captures = [
        local("first", None, json!({"same":1})),
        local("second", None, json!({"same":1.0})),
    ];
    assert!(validate_metadata(captures.iter()).is_ok());
}

#[test]
fn valid_utf16_anchors_rebase_while_ambiguous_or_stale_anchors_clear() {
    let source = local("👩 @build", None, json!({}));
    let mut item = source.item().clone();
    item.prompt_parts.agents.push(QueueAgent {
        name: "build".into(),
        mention: Some(QueueMention {
            start: 3,
            end: 9,
            text: "@build".into(),
        }),
    });
    let source =
        QueueLocalCapture::new("/project", item, Vec::new(), sha256).expect("anchored source");
    let projected = project(
        [&source].into_iter(),
        &[
            ProjectionPiece::Literal("λ ".into()),
            ProjectionPiece::Source(0),
        ],
    )
    .expect("UTF-16 offset mapping");
    let anchor = projected[0].item().prompt_parts.agents[0]
        .mention
        .as_ref()
        .expect("valid anchor retained");
    assert_eq!((anchor.start, anchor.end), (5, 11));
    let repeated = project(
        projected.iter(),
        &[ProjectionPiece::Source(0), ProjectionPiece::Source(0)],
    )
    .expect("complete duplicated text");
    assert!(repeated[0].item().prompt_parts.agents[0].mention.is_none());
    assert_eq!(repeated[0].item().prompt_parts.agents[0].name, "build");
}

#[test]
fn prepared_output_cannot_change_existing_context_ids_or_owned_file_bindings() {
    let original = local("source", Some("msg_original"), json!({"source":true}));
    let mut item = original.item().clone();
    item.text = "expanded once".into();
    let valid = QueueLocalCapture::new(
        "/project",
        item.clone(),
        original.context_ids().to_vec(),
        sha256,
    )
    .expect("prepared source");
    let (accepted, extra) =
        accept_capture(&original, valid, &MessageId::from("msg_skill_reserved"))
            .expect("exact original context survives");
    assert_eq!(accepted.context_ids(), original.context_ids());
    assert!(extra.is_empty());
    item.prompt_parts.context[0].id = Some(MessageId::from("msg_replacement"));
    let invalid = QueueLocalCapture::new(
        "/project",
        item,
        vec![MessageId::from("msg_replacement")],
        sha256,
    )
    .expect("well-formed but foreign identity");
    assert!(accept_capture(&original, invalid, &MessageId::from("msg_skill_reserved")).is_err());
}

#[test]
fn unresolved_skill_instruction_is_explicit_derived_output_with_the_callers_original_id() {
    let original = local("$unresolved", Some("msg_original"), json!({}));
    let reserved = MessageId::from("msg_skill_reserved");
    let mut item = original.item().clone();
    item.prompt_parts.context.push(QueuePromptContext {
        id: Some(reserved.clone()),
        text: "skill instruction".into(),
        description: None,
        metadata: None,
    });
    let prepared = QueueLocalCapture::new(
        "/project",
        item,
        vec![MessageId::from("msg_original"), reserved.clone()],
        sha256,
    )
    .expect("one derived instruction");
    let (retained, derived) =
        accept_capture(&original, prepared, &reserved).expect("explicit caller-bound instruction");
    assert_eq!(retained.context_ids(), original.context_ids());
    assert_eq!(derived[0].id.as_ref(), Some(&reserved));
    assert_eq!(
        original.item().prompt_parts.context.len(),
        1,
        "the immutable original source is unchanged"
    );
}

#[tokio::test]
async fn owning_plan_rebinds_only_the_admitted_companion_context_occurrence() {
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/").expect("inert endpoint"),
        Credential::None,
    )
    .expect("captured client");
    let (owner, server) = captured_server(client, "/project");
    let original = local(
        "original delivered",
        Some("msg_original"),
        json!({"source":true}),
    );
    let original_bytes = original.bytes().to_vec();
    let mut input = delivery_input(&owner, "native source");
    input
        .inputs
        .companions
        .push(ComposerCompanionInput::Edited {
            capture: Box::new(original),
            files: Vec::new(),
            reprocess_text: false,
        });
    input.inputs.text.extend([
        ComposerTextPiece::Literal {
            text: " | ".into(),
            edited: false,
        },
        ComposerTextPiece::Source(1),
    ]);
    let effective = MessageId::from("msg_independent_occurrence");
    let plan = plan_delivery(
        &owner,
        &server.client,
        server.scope.directory(),
        input,
        vec![vec![effective.clone()]],
    )
    .await
    .expect("the already-admitted occurrence binds only the working local capture");
    let working = &plan.sources[1].source.capture;
    assert_eq!(working.context_ids(), std::slice::from_ref(&effective));
    assert_eq!(
        working.item().prompt_parts.context[0].id.as_ref(),
        Some(&effective)
    );
    assert_eq!(
        working.item().prompt_parts.context[0].text,
        "context msg_original"
    );
    assert_eq!(working.item().text, "original delivered");
    let original = QueueLocalCapture::from_bytes(original_bytes, "/project", sha256)
        .expect("immutable original remains valid");
    assert_eq!(original.context_ids(), [MessageId::from("msg_original")]);
    assert_eq!(
        working.item().prompt_parts.metadata,
        original.item().prompt_parts.metadata
    );
    let delivery = finish_delivery(plan, HashMap::new())
        .await
        .expect("independent occurrence finishes without new preparation");
    assert_eq!(delivery.context_ids, std::slice::from_ref(&effective));
    assert_frozen_delivery(&server, &delivery);
    let (capture, files) = delivery
        .into_capture()
        .expect("combined prepared input retains the admitted context order");
    assert!(files.is_empty());
    assert_eq!(capture.context_ids(), [effective]);
    assert_eq!(capture.item().text, "native source | original delivered");
}

fn captured_server(client: ServerClient, directory: &str) -> (DraftKey, QueueServer) {
    let runtime = RuntimeId::from("delivery-owner");
    let session = SessionId::from("ses_delivery");
    let owner = DraftKey::new(
        runtime.clone(),
        directory,
        DraftTarget::Session(session.clone()),
    )
    .expect("normalized draft owner");
    let server = QueueServer::new(
        runtime,
        client,
        tokio::runtime::Handle::current(),
        session,
        directory,
    )
    .expect("exact captured queue server");
    assert_eq!(server.scope.directory(), directory);
    (owner, server)
}

fn delivery_input(owner: &DraftKey, text: &str) -> ComposerDeliveryInput {
    ComposerDeliveryInput {
        inputs: ComposerInputs {
            native: QueueNativeSource {
                owner: owner.clone(),
                draft: Draft::new(text),
            },
            companions: Vec::new(),
            text: vec![ComposerTextPiece::Source(0)],
            exclusions: Vec::new(),
        },
        configuration: configuration(),
        native_context_ids: Vec::new(),
        native_files: Vec::new(),
        skill_context_ids: vec![MessageId::from("msg_native_skill")],
    }
}

fn assert_frozen_delivery(server: &QueueServer, delivery: &ComposerDelivery) {
    let sources = delivery
        .sources
        .iter()
        .map(|source| {
            assert!(
                source.files.is_empty(),
                "these source-only fixtures need no staged byte bindings"
            );
            QueueFrozenSource::Local(QueueLocalFrozenItem {
                capture: &source.capture,
                files: &[],
            })
        })
        .collect::<Vec<_>>();
    let message = MessageId::from("msg_exact_delivery");
    let frozen = QueueFrozenSend::capture_local(
        &server.client,
        QueueLocalSendInput {
            session: server.scope.session(),
            directory: server.scope.directory(),
            message_id: &message,
            sources: &sources,
            selected_input: 0,
            delivery: None,
        },
        sha256,
    )
    .expect("the actual owning-plan output freezes under the captured server's exact directory");
    assert_eq!(frozen.directory(), server.scope.directory());
    let prompt = frozen.prompt_body().to_vec();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&prompt).expect("frozen prompt")["text"],
        delivery.text
    );
    let digest = frozen.sha256();
    let configuration = frozen.configuration();
    let restored = QueueFrozenSend::from_bytes(
        &server.client,
        QueueFrozenSendOwner {
            session: server.scope.session(),
            directory: server.scope.directory(),
            message_id: &message,
            configuration: &configuration,
        },
        frozen.into_bytes(),
        digest,
        sha256,
    )
    .expect("exact captured directory survives frozen-send restoration");
    assert_eq!(restored.directory(), server.scope.directory());
    assert_eq!(restored.prompt_body(), prompt);
}

async fn assert_native_plan_freezes(directory: &str) {
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/captured/").expect("inert endpoint"),
        Credential::None,
    )
    .expect("captured client");
    let (owner, server) = captured_server(client, directory);
    assert_ne!(
        owner.directory(),
        server.scope.directory(),
        "the fixture exercises a normalized draft key"
    );
    // Plain text has no catalog, snippet or file requests. The owning planner
    // chooses every capture directory itself.
    let plan = plan_delivery(
        &owner,
        &server.client,
        server.scope.directory(),
        delivery_input(&owner, "native source"),
        Vec::new(),
    )
    .await
    .expect("plain owning plan");
    assert_eq!(plan.owner, owner);
    let source_directory = plan.sources[0].source.capture.directory().to_owned();
    let reparsed_directory = plan.sources[0]
        .edit
        .as_ref()
        .expect("native edit plan")
        .source()
        .directory()
        .to_owned();
    let delivery = finish_delivery(plan, HashMap::new())
        .await
        .expect("plain completed delivery");
    assert_frozen_delivery(&server, &delivery);
    assert_eq!(
        source_directory,
        server.scope.directory(),
        "native capture retains the submitted directory spelling"
    );
    assert_eq!(
        reparsed_directory,
        server.scope.directory(),
        "source reparsing uses the same exact directory"
    );
    assert_eq!(
        delivery.sources[0].capture.directory(),
        server.scope.directory()
    );
}

#[tokio::test]
async fn owning_plan_native_trailing_slash_directory_reaches_exact_owner_freeze() {
    assert_native_plan_freezes("/project/").await;
}

#[tokio::test]
async fn owning_plan_native_windows_directory_reaches_exact_owner_freeze() {
    assert_native_plan_freezes(r"C:\project").await;
}

#[tokio::test]
async fn owning_plan_refuses_companion_directory_aliases_before_preparation() {
    let client = ServerClient::new(
        Endpoint::parse("https://example.invalid/captured/").expect("inert endpoint"),
        Credential::None,
    )
    .expect("captured client");
    for directory in ["/project/", r"C:\project"] {
        let (owner, server) = captured_server(client.clone(), directory);
        for reprocess_text in [false, true] {
            let mut input = delivery_input(&owner, "native source");
            let companion = QueueLocalCapture::new(
                owner.directory(),
                QueueLocalItem {
                    content: "retained companion".into(),
                    text: "delivered companion".into(),
                    files: Vec::new(),
                    send_config: configuration(),
                    prompt_parts: QueuePromptParts::default(),
                },
                Vec::new(),
                sha256,
            )
            .expect("same normalized owner with a different exact transport directory");
            input
                .inputs
                .companions
                .push(ComposerCompanionInput::Edited {
                    capture: Box::new(companion),
                    files: Vec::new(),
                    reprocess_text,
                });
            input.inputs.text.extend([
                ComposerTextPiece::Literal {
                    text: " | ".into(),
                    edited: false,
                },
                ComposerTextPiece::Source(1),
            ]);
            input
                .skill_context_ids
                .push(MessageId::from("msg_companion_skill"));
            assert!(
                matches!(
                    plan_delivery(
                        &owner,
                        &server.client,
                        server.scope.directory(),
                        input,
                        vec![Vec::new()]
                    )
                    .await,
                    Err(ComposerDeliveryError::Capture(QueueCaptureFailure::Invalid))
                ),
                "every companion must retain the server's exact directory, including untouched delivered text"
            );
        }
    }
}

#[derive(Clone)]
struct PreparationRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl PreparationRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Only agent catalogs and mapped snippet expansion are available. No real
/// OpenCode location, file reads, mutation routes or journal worker is started.
struct PreparationServer {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<PreparationRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl PreparationServer {
    async fn start(directory: &str) -> Self {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("isolated loopback listener");
        let endpoint = Endpoint::parse(&format!(
            "http://{}/captured/",
            listener.local_addr().expect("bound address")
        ))
        .expect("captured endpoint prefix");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let directory = directory.to_owned();
        let task = tokio::spawn(async move {
            // Three processed constituents each read agents once and expand once.
            for _ in 0..6 {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .expect("bounded fixture accept")
                        .expect("accepted fixture request");
                let request = tokio::time::timeout(
                    Duration::from_secs(5),
                    read_preparation_request(&mut socket),
                )
                .await
                .expect("bounded fixture request");
                let path = request.target.split('?').next().expect("request path");
                let (status, body) = match (request.method.as_str(), path) {
                    ("GET", "/captured/api/agent") => (
                        200,
                        json!({"location":{"directory":directory},
                        "data":[{"id":"review","name":"review","mode":"subagent"}]}),
                    ),
                    ("POST", "/captured/api/config/snippets/expand") => {
                        let body: serde_json::Value =
                            serde_json::from_slice(&request.body).expect("snippet request JSON");
                        assert_eq!(
                            body["sourceMap"], true,
                            "the owning finisher requests actual source ranges"
                        );
                        let text = body["text"].as_str().expect("snippet source text");
                        (
                            200,
                            json!({"text":text.replace('#', "expanded-"),
                            "sourceRanges":[{"inputStart":0,"inputEnd":8,"outputStart":0,"outputEnd":8}]}),
                        )
                    }
                    _ => (404, json!({})),
                };
                recorded.lock().expect("fixture request lock").push(request);
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                tokio::time::timeout(
                    Duration::from_secs(5),
                    socket.write_all(response.as_bytes()),
                )
                .await
                .expect("bounded fixture reply")
                .expect("complete fixture reply");
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<PreparationRequest> {
        self.requests.lock().expect("fixture request lock").clone()
    }
}

impl Drop for PreparationServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_preparation_request(socket: &mut tokio::net::TcpStream) -> PreparationRequest {
    use tokio::io::AsyncReadExt;

    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(socket.read_u8().await.expect("request header byte"));
        assert!(header.len() < 65_536, "fixture headers are bounded");
    }
    let header = String::from_utf8(header).expect("ASCII request headers");
    let mut lines = header.lines();
    let mut first = lines.next().expect("request line").split_whitespace();
    let method = first.next().expect("request method").to_owned();
    let target = first.next().expect("request target").to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
        .collect::<Vec<_>>();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| {
            value.parse::<usize>().expect("request content length")
        });
    assert!(length < 65_536, "fixture bodies are bounded");
    let mut body = vec![0; length];
    socket
        .read_exact(&mut body)
        .await
        .expect("complete request body");
    PreparationRequest {
        method,
        target,
        headers,
        body,
    }
}

fn assert_preparation_directories(
    fixture: &PreparationServer,
    directory: &str,
    encoded_directory: &str,
) {
    for request in fixture.requests() {
        assert_eq!(
            request.header("x-opencode-directory"),
            Some(encoded_directory),
            "the owning plan's read retains the exact captured server directory"
        );
        assert_eq!(request.header("x-opencode-directory-encoding"), Some("uri"));
        let route = request
            .target
            .strip_prefix("/captured/")
            .expect("captured endpoint prefix");
        let url = fixture.endpoint.url(route);
        let query = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        if request.method == "POST" {
            assert_eq!(url.path(), "/captured/api/config/snippets/expand");
            assert_eq!(query, vec![("directory".to_owned(), directory.to_owned())]);
        } else {
            assert_eq!(request.method, "GET");
            assert_eq!(url.path(), "/captured/api/agent");
            assert!(query.is_empty());
            assert!(request.body.is_empty());
        }
    }
}

async fn assert_prepared_plan_freezes(directory: &str, encoded_directory: &str) {
    let fixture = PreparationServer::start(directory).await;
    let client =
        ServerClient::new(fixture.endpoint.clone(), Credential::None).expect("fixture client");
    let (owner, server) = captured_server(client, directory);
    let mut input = delivery_input(&owner, "@review #native");
    let companion = QueueLocalCapture::new(
        server.scope.directory(),
        QueueLocalItem {
            content: "@review #companion".into(),
            text: "stale companion delivery".into(),
            files: Vec::new(),
            send_config: configuration(),
            prompt_parts: QueuePromptParts::default(),
        },
        Vec::new(),
        sha256,
    )
    .expect("edited companion retains the exact captured directory");
    let companion_bytes = companion.bytes().to_vec();
    input
        .inputs
        .companions
        .push(ComposerCompanionInput::Edited {
            capture: Box::new(companion),
            files: Vec::new(),
            reprocess_text: true,
        });
    input.inputs.text.extend([
        ComposerTextPiece::Literal {
            text: " | ".into(),
            edited: false,
        },
        ComposerTextPiece::Source(1),
        ComposerTextPiece::Literal {
            text: " | ".into(),
            edited: false,
        },
        ComposerTextPiece::Literal {
            text: "@review #literal".into(),
            edited: true,
        },
    ]);
    input.skill_context_ids.extend([
        MessageId::from("msg_companion_skill"),
        MessageId::from("msg_literal_skill"),
    ]);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        plan_delivery(
            &owner,
            &server.client,
            server.scope.directory(),
            input,
            vec![Vec::new()],
        ),
    )
    .await
    .expect("bounded owning plan");
    assert_preparation_directories(&fixture, server.scope.directory(), encoded_directory);
    let plan = result
        .expect("owning plan accepts exact-directory native, edited companion and edited literal");
    assert_eq!(plan.owner, owner);
    assert_eq!(
        fixture.requests().len(),
        3,
        "each processed constituent reads only its captured agent catalog"
    );
    assert_eq!(
        plan.sources[1].source.capture.bytes(),
        companion_bytes,
        "planning preserves the companion's immutable capture"
    );
    let requests = plan.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.identity)
            .collect::<Vec<_>>(),
        vec![
            QueuePreparationSourceId::Source(0),
            QueuePreparationSourceId::Source(1),
            QueuePreparationSourceId::Literal(4),
        ]
    );
    for request in requests {
        assert_eq!(
            request.capture.directory(),
            server.scope.directory(),
            "all reparsed sources and constructed literals keep exact scope"
        );
        assert!(
            request.requests.is_empty(),
            "agent mentions create no file-read authority"
        );
    }
    let delivery = tokio::time::timeout(
        Duration::from_secs(5),
        finish_delivery(plan, HashMap::new()),
    )
    .await
    .expect("bounded owning finisher")
    .expect("complete mapped expansions");
    assert_preparation_directories(&fixture, server.scope.directory(), encoded_directory);
    let requests = fixture.requests();
    assert_eq!(
        requests.len(),
        6,
        "only the three catalog reads and three expansions are issued"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .map(
                |request| serde_json::from_slice::<serde_json::Value>(&request.body)
                    .expect("expansion body")
            )
            .collect::<Vec<_>>(),
        vec![
            json!({"text":"@review #native","sourceMap":true}),
            json!({"text":"@review #companion","sourceMap":true}),
            json!({"text":"@review #literal","sourceMap":true}),
        ]
    );
    assert_eq!(
        delivery.content,
        "@review #native | @review #companion | @review #literal"
    );
    assert_eq!(
        delivery.text,
        "@review expanded-native | @review expanded-companion | @review expanded-literal"
    );
    assert!(delivery.derived_contexts.is_empty());
    for source in &delivery.sources {
        assert_eq!(source.capture.directory(), server.scope.directory());
    }
    assert_frozen_delivery(&server, &delivery);
    let (combined, files) = delivery
        .into_capture()
        .expect("enqueue and prefix combined capture");
    assert_eq!(combined.directory(), server.scope.directory());
    assert!(files.is_empty());
}

#[tokio::test]
async fn owning_plan_preparation_normalized_directory_control_reaches_freeze() {
    assert_prepared_plan_freezes("/project", "%2Fproject").await;
}

#[tokio::test]
async fn owning_plan_preparation_trailing_slash_directory_reaches_freeze() {
    assert_prepared_plan_freezes("/project/", "%2Fproject%2F").await;
}

#[tokio::test]
async fn owning_plan_preparation_windows_directory_reaches_freeze() {
    assert_prepared_plan_freezes(r"C:\project", "C%3A%5Cproject").await;
}
