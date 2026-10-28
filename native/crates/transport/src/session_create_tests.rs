use serde_json::Value;

use crate::credential::Credential;
use crate::endpoint::Endpoint;

use super::*;

const ID: &str = "ses_e9b49c443ffefXcWWPhCOga6nH";
const DIRECTORY: &str = "/home/synthetic/my project/ünï";

fn request(model: Option<ModelChoice>) -> CreateSession {
    CreateSession::new(SessionId::new(ID), DIRECTORY, model, None).expect("valid creation")
}

#[test]
fn creation_and_exact_lookup_match_the_2_2_sdk() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/transport.json"))
        .expect("generated fixture");
    let client = ServerClient::new(
        Endpoint::parse("http://127.0.0.1:4799").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let mut explicit = request(Some(ModelChoice {
        provider_id: "fixture".to_owned(),
        model_id: "text-model".to_owned(),
        variant: Some("high".to_owned()),
    }));
    explicit.agent = Some("build".to_owned());
    for (name, builder) in [
        (
            "create root session",
            client.create_session_request(&explicit),
        ),
        (
            "create root session with defaults",
            client.create_session_request(&request(None)),
        ),
        (
            "observe exact session",
            client.exact_session_request(explicit.id()),
        ),
    ] {
        let expected = fixture["requests"]
            .as_array()
            .expect("requests")
            .iter()
            .find(|entry| entry["name"] == name)
            .expect("SDK operation");
        let actual = builder.build().expect("request");
        assert_eq!(actual.method().as_str(), expected["method"], "{name}");
        assert_eq!(actual.url().path(), expected["path"], "{name}");
        assert!(actual.url().query().is_none());
        assert!(actual.headers().get("x-opencode-directory").is_none());
        assert!(expected["directoryHeader"].is_null());
        assert_eq!(
            actual
                .headers()
                .get("content-type")
                .map(|value| value.to_str().expect("header")),
            expected["contentType"].as_str()
        );
        let body = actual
            .body()
            .and_then(|body| body.as_bytes())
            .map(|bytes| serde_json::from_slice::<Value>(bytes).expect("JSON"));
        let expected_body = expected["body"]
            .as_str()
            .map(|body| serde_json::from_str::<Value>(body).expect("SDK JSON"));
        assert_eq!(body, expected_body, "{name}");
    }
}

#[test]
fn creation_rejects_relative_paths_and_omits_auto_and_parent() {
    for directory in [
        "",
        "relative",
        "C:relative",
        "/a/../b",
        "/a/./b",
        " /a",
        "/a\n",
        "//server",
    ] {
        assert!(
            CreateSession::new(SessionId::new(ID), directory, None, None).is_err(),
            "{directory:?}"
        );
    }
    for directory in ["/", "C:/", "C:\\project", "//server/share", DIRECTORY] {
        assert!(
            CreateSession::new(SessionId::new(ID), directory, None, None).is_ok(),
            "{directory:?}"
        );
    }
    for id in ["", "msg_wrong", "ses_", "ses_a/b", "ses_a?b"] {
        assert!(CreateSession::new(SessionId::new(id), DIRECTORY, None, None).is_err());
    }
    let auto = request(Some(ModelChoice {
        provider_id: "openchamber".to_owned(),
        model_id: "auto".to_owned(),
        variant: None,
    }));
    assert!(auto.model().is_none());
    let client = ServerClient::new(
        Endpoint::parse("http://localhost:4799/prefix/").expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let built = client
        .create_session_request(&auto)
        .build()
        .expect("request");
    assert_eq!(built.url().path(), "/prefix/api/session");
    let body: Value =
        serde_json::from_slice(built.body().expect("body").as_bytes().expect("bytes"))
            .expect("JSON");
    assert!(body.get("model").is_none());
    assert!(body.get("parentID").is_none());
    assert!(body.get("agent").is_none());
}

#[test]
fn creation_receipts_require_exact_identity_and_normalized_absolute_location() {
    let sample: Value =
        serde_json::from_str(include_str!("../tests/fixtures/wire/session-created.json"))
            .expect("sample");
    for (id, directory, expected) in [
        (ID, DIRECTORY.to_owned(), "ok"),
        (ID, format!("{DIRECTORY}/"), "ok"),
        ("ses_unrelated", DIRECTORY.to_owned(), "id"),
        (ID, "/unrelated".to_owned(), "location"),
        (ID, "relative".to_owned(), "location"),
    ] {
        let mut value = sample.clone();
        value["data"]["id"] = id.into();
        value["data"]["location"]["directory"] = directory.into();
        let reply: SessionResponse = serde_json::from_value(value).expect("wire");
        let result = verify_session(reply.data, &SessionId::new(ID), DIRECTORY);
        match expected {
            "ok" => assert!(result.is_ok()),
            "id" => assert!(matches!(result, Err(SessionError::IdentityMismatch))),
            _ => assert!(matches!(result, Err(SessionError::LocationMismatch))),
        }
    }
}
