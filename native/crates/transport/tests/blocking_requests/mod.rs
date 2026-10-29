//! Blocking-request contracts against a real scripted loopback HTTP server.
use super::{Reply, Server, read_request};
use openchamber_native_core::form::{FormAnswer, FormFieldKind, FormState, FormValue};
use openchamber_native_core::ids::{FormId, PermissionId, RuntimeId};
use openchamber_native_core::pending_requests::{
    PendingRequest, PendingRequests, RequestId, RequestKey, RequestKind,
};
use openchamber_native_core::permission::PermissionReply;
use openchamber_native_core::request_list::RequestListIssueKind;
use openchamber_native_core::request_owner::{RequestOwner, RequestScope};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::{self, BlockingRequestEvent, ServerEvent};
use openchamber_native_transport::requests::RequestError;
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn sample() -> Value {
    serde_json::from_str(include_str!("../fixtures/wire/blocking-requests.json"))
        .expect("synthetic sample")
}

#[tokio::test]
async fn host_status_pending_hints_preserve_optional_support_and_isolate_invalid_entries() {
    for supported in [false, true] {
        let server = Server::start(move |request| {
            assert_eq!(request.target, "/prefix/api/sessions/status");
            let mut snapshot = json!({"sessions":{"ses_owner":{"status":"busy"}},"serverTime":1});
            if supported { snapshot["pending"] = json!({
                "ses_owner":{"permissions":[{"id":"per_good","sessionID":"ses_owner"},{"id":"per_wrong","sessionID":"ses_other"},null],"forms":[{"id":"frm_good","sessionID":"ses_owner"}]},
                "global":{"permissions":[{"id":"per_global","sessionID":"global"}],"forms":[{"id":"frm_global","sessionID":"global"}]}
            }); }
            Reply::json(snapshot)
        }, vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let snapshot = client.host_session_status().await.expect("host snapshot");
        assert_eq!(snapshot.statuses.len(), 1);
        if supported {
            let pending = snapshot.pending.expect("supported");
            assert_eq!(pending.len(), 3, "valid siblings survive invalid hints");
            assert!(
                pending
                    .iter()
                    .any(|stub| stub.owner == RequestOwner::Location
                        && matches!(stub.id, RequestId::Form(_)))
            );
        } else {
            assert!(snapshot.pending.is_none(), "omitted pending is unsupported");
        }
    }
}
fn scope() -> RequestScope {
    RequestScope::new(
        RuntimeId::from("synthetic-runtime"),
        "/home/synthetic/my project/ünï",
    )
    .expect("scope")
}
fn key(form: bool, global: bool) -> RequestKey {
    RequestKey {
        scope: scope(),
        owner: RequestOwner::from_wire(if global {
            "global"
        } else {
            "ses_native_fixture"
        })
        .expect("owner"),
        id: if form {
            RequestId::Form(FormId::from("frm_native_fixture"))
        } else {
            RequestId::Permission(PermissionId::from("per_native_fixture"))
        },
    }
}
fn reply(request: &super::Recorded) -> Reply {
    let sample = sample();
    if request.method != "GET" {
        return Reply::with_status(204, "application/json", "");
    }
    let global = request.target.contains("/global/");
    let mut form = sample["form"].clone();
    if global {
        form["sessionID"] = "global".into();
    }
    if request.target.ends_with("/permission/request") {
        return Reply::json(
            json!({"location":{"directory":scope().directory()},"data":[sample["permission"]]}),
        );
    }
    if request.target == "/prefix/api/form" {
        let mut global_form = form.clone();
        global_form["sessionID"] = "global".into();
        global_form["id"] = "frm_global_fixture".into();
        return Reply::json(
            json!({"location":{"directory":scope().directory()},"data":[form,global_form]}),
        );
    }
    if request.target.ends_with("/permission") {
        return Reply::json(json!({"data":[sample["permission"]]}));
    }
    if request.target.ends_with("/form") {
        return Reply::json(json!({"data":[form]}));
    }
    if request.target.contains("/permission/") {
        return Reply::json(json!({"data":sample["permission"]}));
    }
    form["state"] = json!({"status":"pending"});
    Reply::json(json!({"data":form}))
}
fn answers() -> FormAnswer {
    FormAnswer::from([
        ("q0".into(), FormValue::String("A (Recommended)".into())),
        (
            "q1".into(),
            FormValue::Strings(vec!["one".into(), "custom".into()]),
        ),
        (
            "count".into(),
            FormValue::Number(openchamber_native_core::form::FormNumber::new(2.0).expect("finite")),
        ),
        ("enabled".into(), FormValue::Boolean(false)),
        ("link".into(), FormValue::Boolean(true)),
    ])
}

fn fixture_answer(value: &Value) -> FormAnswer {
    value
        .as_object()
        .expect("answer")
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => FormValue::String(text.clone()),
                Value::Bool(value) => FormValue::Boolean(*value),
                Value::Number(value) => FormValue::Number(
                    openchamber_native_core::form::FormNumber::new(value.as_f64().expect("number"))
                        .expect("finite"),
                ),
                Value::Array(values) => FormValue::Strings(
                    values
                        .iter()
                        .map(|v| v.as_str().expect("string").to_owned())
                        .collect(),
                ),
                _ => panic!("invalid fixture answer"),
            };
            (key.clone(), value)
        })
        .collect()
}

#[tokio::test]
async fn official_fixture_requests_match_auth_prefix_global_headers_and_bodies() {
    let server = Server::start(reply, vec![]).await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("synthetic-token".into()),
    )
    .expect("client");
    let permission = key(false, false);
    let form = key(true, false);
    let global = key(true, true);
    let permissions = client
        .pending_permissions(&scope())
        .await
        .expect("location permissions");
    assert!(permissions.is_complete());
    let permissions = permissions.entries();
    assert_eq!(permissions.len(), 1);
    assert!(permissions[0].can_save());
    assert_eq!(
        permissions[0].source.as_ref().expect("source").call_id,
        "call_fixture"
    );
    let forms = client
        .pending_forms(&scope())
        .await
        .expect("location forms");
    assert!(forms.is_complete());
    let forms = forms.entries();
    assert_eq!(forms.len(), 2);
    assert_eq!(forms[1].owner, RequestOwner::Location);
    assert!(
        matches!(
            forms[0].fields[3].kind,
            FormFieldKind::Number {
                minimum: None,
                maximum: None,
                default: None
            }
        ),
        "numeric sentinels are absent bounds/defaults"
    );
    client
        .session_permissions(&scope(), &permission.owner)
        .await
        .expect("session permissions");
    client
        .session_forms(&scope(), &form.owner)
        .await
        .expect("session forms");
    assert_eq!(
        client
            .permission_request(&permission)
            .await
            .expect("permission"),
        permissions[0]
    );
    assert_eq!(
        client.form_detail(&form).await.expect("form").state,
        FormState::Pending
    );
    assert_eq!(
        client
            .form_detail(&global)
            .await
            .expect("global")
            .form
            .owner,
        RequestOwner::Location
    );
    for decision in [
        PermissionReply::Once,
        PermissionReply::Always,
        PermissionReply::Reject,
    ] {
        client
            .reply_permission(&permission, decision, None)
            .await
            .expect("explicit reply");
    }
    client
        .reply_permission(
            &permission,
            PermissionReply::Reject,
            Some("Use another command"),
        )
        .await
        .expect("feedback");
    client.reply_form(&form, &answers()).await.expect("answer");
    client
        .reply_form(&global, &answers())
        .await
        .expect("global answer");
    client.cancel_form(&form, None).await.expect("cancel");
    client
        .cancel_form(&global, None)
        .await
        .expect("global cancel");
    client
        .cancel_form(&form, Some("No answer available"))
        .await
        .expect("cancel feedback");
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/transport.json")).expect("official fixture");
    let defaults = &fixture["blockingRequests"]["formDefaults"];
    let expected_answer: FormAnswer = defaults["answer"]
        .as_object()
        .expect("defaults")
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => FormValue::String(text.clone()),
                Value::Bool(value) => FormValue::Boolean(*value),
                Value::Number(number) => FormValue::Number(
                    openchamber_native_core::form::FormNumber::new(
                        number.as_f64().expect("number"),
                    )
                    .expect("finite"),
                ),
                Value::Array(values) => FormValue::Strings(
                    values
                        .iter()
                        .map(|v| v.as_str().expect("string").to_owned())
                        .collect(),
                ),
                _ => panic!("invalid fixture answer"),
            };
            (key.clone(), value)
        })
        .collect();
    let (active, answer) = forms[0].evaluate(&forms[0].defaults());
    assert_eq!(
        answer, expected_answer,
        "defaults match Electron's pure form state"
    );
    assert_eq!(
        json!(
            active
                .iter()
                .map(|field| field.key.as_str())
                .collect::<Vec<_>>()
        ),
        defaults["visible"]
    );
    assert_eq!(defaults["missing"], json!(["link"]));
    let expected = fixture["blockingRequests"]["requests"]
        .as_array()
        .expect("request fixtures");
    let actual = server.requests();
    assert_eq!(actual.len(), expected.len(), "no hidden mutation or retry");
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.method, expected["method"], "{}", expected["name"]);
        let url = reqwest::Url::parse(&format!("http://synthetic.invalid{}", actual.target))
            .expect("target");
        assert_eq!(
            url.path().strip_prefix("/prefix").expect("prefix"),
            expected["path"]
        );
        let query: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| vec![k.into_owned(), v.into_owned()])
            .collect();
        assert_eq!(json!(query), expected["query"]);
        assert_eq!(
            actual.header("authorization"),
            Some("Bearer synthetic-token")
        );
        assert_eq!(
            json!(actual.header("x-opencode-directory")),
            expected["directoryHeader"]
        );
        assert_eq!(
            json!(actual.header("content-type")),
            expected["contentType"]
        );
        let body = if actual.body.is_empty() {
            Value::Null
        } else {
            actual.json()
        };
        assert_eq!(body, expected["body"], "{}", expected["name"]);
    }
}

#[tokio::test]
async fn declared_errors_require_matching_tag_status_kind_and_identity() {
    for (status, tag, id, expected) in [
        (
            404,
            "PermissionNotFoundError",
            "per_native_fixture",
            "permission",
        ),
        (404, "FormNotFoundError", "frm_native_fixture", "form"),
        (
            409,
            "FormAlreadySettledError",
            "frm_native_fixture",
            "settled",
        ),
        (
            400,
            "FormInvalidAnswerError",
            "frm_native_fixture",
            "invalid",
        ),
        (404, "SessionNotFoundError", "ses_native_fixture", "session"),
        (404, "LocationNotFoundError", "unused", "location"),
        (404, "FormNotFoundError", "frm_other", "http"),
        (404, "UnknownError", "frm_native_fixture", "http"),
        (400, "FormAlreadySettledError", "frm_native_fixture", "http"),
    ] {
        let server = Server::start(move |_| Reply::with_status(status, "application/json", json!({"_tag":tag,"id":id,"requestID":id,"sessionID":id,"message":"private response text"}).to_string()), vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let error = if expected == "permission" {
            client
                .reply_permission(&key(false, false), PermissionReply::Once, None)
                .await
                .expect_err("not found")
        } else {
            client
                .cancel_form(&key(true, false), None)
                .await
                .expect_err("declared refusal")
        };
        assert!(!format!("{error:?}").contains("private response text"));
        let actual = match error {
            RequestError::PermissionNotFound => "permission",
            RequestError::FormNotFound => "form",
            RequestError::FormAlreadySettled => "settled",
            RequestError::FormInvalidAnswer => "invalid",
            RequestError::SessionNotFound => "session",
            RequestError::LocationNotFound => "location",
            RequestError::Transport(TransportError::Http { .. }) => "http",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, expected);
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn failed_lists_wrong_owners_and_malformed_forms_never_become_empty_success() {
    for case in ["owner", "id", "location", "field", "auth", "network-status"] {
        let server = Server::start(
            move |request| {
                if case == "auth" {
                    return Reply::with_status(401, "application/json", "{}");
                }
                if case == "network-status" {
                    return Reply::with_status(503, "application/json", "{}");
                }
                let mut response = reply(request);
                let mut value: Value = serde_json::from_str(&response.body).expect("json");
                match case {
                    "owner" => value["data"]["sessionID"] = "ses_wrong".into(),
                    "id" => value["data"]["id"] = "frm_wrong".into(),
                    "location" => value["location"]["directory"] = "/wrong".into(),
                    "field" => {
                        value["data"]["fields"][1]["when"] =
                            json!([{"key":"later","op":"eq","value":"bad"}])
                    }
                    _ => unreachable!(),
                }
                response.body = value.to_string();
                response
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let result = if matches!(case, "location" | "auth" | "network-status") {
            client.pending_forms(&scope()).await.map(|_| ())
        } else {
            client.form_detail(&key(true, false)).await.map(|_| ())
        };
        let error = result.expect_err("failed read grants no authority");
        assert!(
            !matches!(
                error,
                RequestError::FormNotFound | RequestError::PermissionNotFound
            ),
            "a get or envelope decoding failure must not become not-found"
        );
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn lists_isolate_bad_entries_in_every_position_and_preserve_valid_siblings() {
    for permission in [false, true] {
        for bad_index in 0..3 {
            let server = Server::start(move |_| {
                let sample = sample();
                let mut entries = Vec::new();
                for index in 0..3 {
                    if index == bad_index { entries.push(json!({"id":"untrusted-guessed-id","private":"SYNTHETIC_SECRET_MARKER"})); }
                    else { let mut entry = sample[if permission { "permission" } else { "form" }].clone();
                        entry["id"] = format!("{}_valid_{index}",if permission { "per" } else { "frm" }).into(); entries.push(entry); }
                }
                Reply::json(json!({"location":{"directory":scope().directory()},"data":entries}))
            },vec![]).await;
            let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
            let list = if permission {
                client
                    .pending_permissions(&scope())
                    .await
                    .expect("partial permissions")
                    .map(PendingRequest::Permission)
            } else {
                client
                    .pending_forms(&scope())
                    .await
                    .expect("partial forms")
                    .map(PendingRequest::Form)
            };
            assert_eq!(list.entries().len(), 2);
            assert!(!list.is_complete());
            assert_eq!(list.issues().len(), 1);
            assert_eq!(list.issues()[0].index, bad_index);
            assert_eq!(list.issues()[0].reason, RequestListIssueKind::Malformed);
            assert!(!format!("{list:?}").contains("untrusted-guessed-id"));
            assert!(!format!("{list:?}").contains("SYNTHETIC_SECRET_MARKER"));
            let mut retained = list.entries()[0].clone();
            match &mut retained {
                PendingRequest::Permission(request) => {
                    request.id = PermissionId::from("per_retained")
                }
                PendingRequest::Form(request) => request.id = FormId::from("frm_retained"),
            }
            let retained_key = retained.key(&scope());
            let mut pending = PendingRequests::new(scope());
            pending.asked(retained);
            let kind = if permission {
                RequestKind::Permission
            } else {
                RequestKind::Form
            };
            let load = pending.begin_load(kind);
            pending
                .apply_list(load, list)
                .expect("publish valid siblings");
            assert_eq!(pending.entries().count(), 3);
            assert!(
                pending.get(&retained_key).is_some(),
                "incomplete lists cannot remove held requests"
            );
            assert!(!pending.is_complete(kind));
        }
    }
}

#[tokio::test]
async fn duplicate_and_wrong_owner_entries_are_excluded_without_hiding_siblings() {
    for permission in [false, true] {
        let server = Server::start(
            move |_| {
                let sample = sample();
                let base = sample[if permission { "permission" } else { "form" }].clone();
                let mut before = base.clone();
                before["id"] = if permission {
                    "per_before"
                } else {
                    "frm_before"
                }
                .into();
                let mut wrong = base.clone();
                wrong["id"] = if permission { "per_wrong" } else { "frm_wrong" }.into();
                wrong["sessionID"] = "ses_other".into();
                let mut after = base.clone();
                after["id"] = if permission { "per_after" } else { "frm_after" }.into();
                Reply::json(json!({"data":[before,base,wrong,base,after]}))
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let owner = key(true, false).owner;
        let list = if permission {
            client
                .session_permissions(&scope(), &owner)
                .await
                .expect("partial permissions")
                .map(PendingRequest::Permission)
        } else {
            client
                .session_forms(&scope(), &owner)
                .await
                .expect("partial forms")
                .map(PendingRequest::Form)
        };
        assert_eq!(
            list.entries().len(),
            2,
            "neither duplicate is selected as authoritative"
        );
        assert!(!list.is_complete());
        assert_eq!(
            list.issues()
                .iter()
                .map(|issue| (issue.index, issue.reason))
                .collect::<Vec<_>>(),
            vec![
                (1, RequestListIssueKind::DuplicateIdentity),
                (2, RequestListIssueKind::WrongOwner),
                (3, RequestListIssueKind::DuplicateIdentity)
            ]
        );
    }
}

#[test]
fn blocking_event_debug_redacts_transitive_form_and_permission_content() {
    const SECRET: &str = "SYNTHETIC_SECRET_MARKER";
    let mut sample = sample();
    sample["form"]["title"] = SECRET.into();
    sample["form"]["metadata"] = json!({"private":SECRET});
    sample["form"]["fields"][0]["default"] = SECRET.into();
    sample["permission"]["resources"] = json!([SECRET]);
    sample["permission"]["save"] = json!([SECRET]);
    sample["permission"]["metadata"] = json!({"private":SECRET});
    sample["permission"]["message"] = SECRET.into();
    for (kind, data, id) in [
        (
            "form.created",
            json!({"form":sample["form"]}),
            "frm_native_fixture",
        ),
        (
            "permission.asked",
            sample["permission"].clone(),
            "per_native_fixture",
        ),
    ] {
        let decoded = event::decode_frame(
            &json!({"type":"event","payload":{"type":kind,"data":data}}).to_string(),
        )
        .expect("event");
        let debug = format!("{decoded:?}");
        assert!(!debug.contains(SECRET));
        assert!(debug.contains(id));
        assert!(debug.contains("redacted"));
    }
}

#[tokio::test]
async fn defaults_match_electron_for_empty_unicode_and_exact_option_values() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/transport.json")).expect("fixture");
    for case in fixture["blockingRequests"]["defaultParity"]
        .as_array()
        .expect("default cases")
    {
        let mut wire = case["form"].clone();
        wire["state"] = json!({"status":"pending"});
        let server = Server::start(move |_| Reply::json(json!({"data":wire})), vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let mut key = key(true, false);
        key.id = RequestId::Form(FormId::from(case["form"]["id"].as_str().expect("id")));
        let form = client.form_detail(&key).await.expect("typed form").form;
        let (active, answer) = form.evaluate(&form.defaults());
        assert_eq!(answer, fixture_answer(&case["answer"]), "{}", case["name"]);
        assert_eq!(
            json!(
                active
                    .iter()
                    .map(|field| field.key.as_str())
                    .collect::<Vec<_>>()
            ),
            case["visible"],
            "{}",
            case["name"]
        );
    }
}

#[tokio::test]
async fn redirects_never_forward_credentials_or_repeat_mutations() {
    let destination = Server::start(reply, vec![]).await;
    let location = destination.endpoint().url("api/form").to_string();
    let server = Server::start(
        move |_| {
            let mut response = Reply::with_status(307, "application/json", "{}");
            response.headers.push(("Location", location.clone()));
            response
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("private-test-token".into()),
    )
    .expect("client");
    assert!(matches!(
        client.cancel_form(&key(true, true), None).await,
        Err(RequestError::Transport(TransportError::Redirected))
    ));
    assert!(destination.requests().is_empty());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn lost_reply_response_is_an_error_and_never_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let endpoint = Endpoint::parse(&format!(
        "http://{}/prefix",
        listener.local_addr().expect("address")
    ))
    .expect("endpoint");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("connection");
        let received = read_request(&mut socket, &mut Vec::new())
            .await
            .expect("request");
        drop(socket);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "no automatic retry"
        );
        received
    });
    let client = ServerClient::new(endpoint, Credential::None).expect("client");
    assert!(matches!(
        client.reply_form(&key(true, false), &answers()).await,
        Err(RequestError::Transport(_))
    ));
    assert_eq!(server.await.expect("server").method, "POST");
}

#[test]
fn official_nested_events_preserve_scope_and_terminal_owner() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/transport.json")).expect("fixture");
    for entry in fixture["blockingRequests"]["events"]
        .as_array()
        .expect("events")
    {
        let frame = json!({"type":"event","eventId":"bridge_1","payload":entry["payload"]});
        let decoded = event::decode_frame(&frame.to_string()).expect("event");
        assert_eq!(decoded.event_id.as_deref(), Some("bridge_1"));
        assert_eq!(decoded.events.len(), 1);
        let projected_type = match &decoded.events[0] {
            ServerEvent::BlockingRequest { directory, event } => {
                assert_eq!(directory.as_deref(), Some(scope().directory()));
                match event {
                    BlockingRequestEvent::Asked(PendingRequest::Permission(_)) => {
                        "permission.asked"
                    }
                    BlockingRequestEvent::Asked(PendingRequest::Form(form)) => {
                        assert_eq!(
                            form.owner.as_str(),
                            entry["payload"]["data"]["form"]["sessionID"]
                        );
                        "form.created"
                    }
                    BlockingRequestEvent::Settled { owner, id } => {
                        assert_eq!(owner.as_str(), entry["payload"]["data"]["sessionID"]);
                        match id {
                            RequestId::Permission(_) => "permission.replied",
                            RequestId::Form(_) => "form.settled",
                        }
                    }
                }
            }
            ServerEvent::LocationShutdown { directory } => {
                assert_eq!(directory, scope().directory());
                "location.shutdown"
            }
            other => panic!("unhandled {other:?}"),
        };
        assert_eq!(projected_type, entry["projected"][0]["type"]);
    }
    let mut form = sample()["form"].clone();
    form["sessionID"] = "global".into();
    assert!(
        matches!(
            event::decode_frame(
                &json!({"type":"event","payload":{"type":"form.created","data":{"form":form}}})
                    .to_string()
            )
            .expect("frame remains consumable")
            .events
            .as_slice(),
            [ServerEvent::BlockingRequestInvalid {
                directory: None,
                ..
            }]
        ),
        "global forms never inherit current directory"
    );
}

#[tokio::test]
async fn exact_form_reads_keep_terminal_states_and_global_lists_keep_their_directory() {
    for status in ["answered", "cancelled"] {
        let server = Server::start(
            move |request| {
                let mut response = reply(request);
                let mut value: Value = serde_json::from_str(&response.body).expect("json");
                if !request.target.ends_with("/form") {
                    value["data"]["state"] = if status == "answered" {
                        json!({"status":status,"answer":sample()["answer"]})
                    } else {
                        json!({"status":status,"message":"Explicit feedback"})
                    };
                }
                response.body = value.to_string();
                response
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let detail = client
            .form_detail(&key(true, true))
            .await
            .expect("terminal form");
        match detail.state {
            FormState::Answered(answer) => assert_eq!(answer, answers()),
            FormState::Cancelled { message } => {
                assert_eq!(message.as_deref(), Some("Explicit feedback"))
            }
            FormState::Pending => panic!("terminal state lost"),
        }
        assert_eq!(
            client
                .session_forms(&scope(), &RequestOwner::Location)
                .await
                .expect("location owner list")
                .entries()
                .len(),
            1
        );
        for request in server.requests() {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.header("x-opencode-directory"),
                Some("%2Fhome%2Fsynthetic%2Fmy%20project%2F%C3%BCn%C3%AF")
            );
        }
    }
}

#[test]
fn malformed_request_event_preserves_replay_progress_without_mutating_other_requests() {
    let malformed = json!({"type":"event","eventId":"bridge_bad","payload":{
        "type":"form.created","location":{"directory":"/project"},"data":{"form":{"id":"frm_bad"}}
    }});
    let decoded =
        event::decode_frame(&malformed.to_string()).expect("consume malformed entity frame");
    assert_eq!(decoded.event_id.as_deref(), Some("bridge_bad"));
    assert!(
        matches!(decoded.events.as_slice(),[ServerEvent::BlockingRequestInvalid { directory: Some(directory), .. }] if directory == "/project")
    );
    let good = json!({"type":"event","eventId":"bridge_good","payload":{
        "type":"permission.asked","data":sample()["permission"]
    }});
    assert!(matches!(
        event::decode_frame(&good.to_string())
            .expect("unrelated request")
            .events
            .as_slice(),
        [ServerEvent::BlockingRequest {
            directory: None,
            event: BlockingRequestEvent::Asked(PendingRequest::Permission(_))
        }]
    ));
}
