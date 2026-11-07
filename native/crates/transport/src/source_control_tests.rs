use std::sync::{Arc, Mutex};

use openchamber_native_core::source_board::{
    ChecksState, ListFilter, ListQuery, PeopleFilter, ReviewDecision, StateFilter, read_contexts,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

/// A request the fake server received.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: Value,
}

impl Recorded {
    fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

type Route = Box<dyn Fn(&Recorded) -> (u16, Value) + Send + Sync>;

/// A loopback stand-in for the OpenChamber server's source-control routes:
/// it answers by path and records every request. No provider is called.
async fn fake_server(route: Route) -> (ServerClient, Arc<Mutex<Vec<Recorded>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let log = recorded.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let end = loop {
                let mut buffer = [0_u8; 4096];
                let count = socket.read(&mut buffer).await.expect("request");
                assert!(count > 0, "request must have headers");
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let head = String::from_utf8(bytes[..end].to_vec()).expect("ASCII headers");
            assert!(
                head.to_ascii_lowercase()
                    .contains("authorization: bearer oc_client_synthetic"),
                "every request carries the client token"
            );
            let length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("length"))
                })
                .unwrap_or_default();
            while bytes.len() < end + length {
                let mut buffer = [0_u8; 4096];
                let count = socket.read(&mut buffer).await.expect("body");
                bytes.extend_from_slice(&buffer[..count]);
            }
            let line = head.lines().next().expect("request line");
            let mut parts = line.split(' ');
            let method = parts.next().unwrap_or_default().to_owned();
            let target = Url::parse(&format!("http://fake{}", parts.next().unwrap_or("/")))
                .expect("request target");
            let request = Recorded {
                method,
                path: target.path().trim_start_matches("/prefix").to_owned(),
                query: target.query_pairs().into_owned().collect(),
                body: if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + length]).expect("JSON body")
                },
            };
            let (status, body) = route(&request);
            log.lock().expect("log").push(request);
            let body = body.to_string();
            let reply = format!(
                "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    let endpoint = crate::endpoint::Endpoint::parse(&format!("http://{address}/prefix/"))
        .expect("test endpoint");
    let client = ServerClient::new(
        endpoint,
        crate::credential::Credential::ClientToken("oc_client_synthetic".into()),
    )
    .expect("test client");
    (client, recorded)
}

fn context() -> ReadContext {
    ReadContext {
        directory: "/work/project".into(),
        identity: Identity {
            provider: "github".into(),
            instance: "github.com".into(),
        },
        repository_id: "repo-1".into(),
        account_id: "acct-1".into(),
        binding_revision: 0,
        primary_remote: "origin".into(),
    }
}

fn issue(number: u64, state: &str) -> Value {
    json!({
        "kind": "issue", "number": number, "title": format!("Issue {number}"),
        "url": format!("https://github.com/acme/app/issues/{number}"),
        "body": "Body", "bodyTruncated": false, "createdAt": null,
        "updatedAt": "2026-01-01T00:00:00Z", "author": {"login": "octo"},
        "labels": [{"name": "bug", "color": "d73a4a"}], "commentCount": 2,
        "sourceRepo": {"owner": "acme", "repo": "app", "source": "repository"},
        "state": state,
    })
}

fn pull(number: u64) -> Value {
    json!({
        "kind": "pull", "number": number, "title": "Fix", "url": "https://github.com/acme/app/pull/7",
        "body": "", "bodyTruncated": false, "createdAt": null, "updatedAt": null, "author": null,
        "labels": [], "commentCount": 0,
        "sourceRepo": {"owner": "acme", "repo": "app", "source": "upstream"},
        "state": "open", "draft": true, "head": "fix", "base": "main", "headSha": "abc123",
        "headRepo": null,
    })
}

fn receipt(number: u64, result: Value) -> Value {
    json!({
        "status": "succeeded", "replayed": false, "result": result,
        "actor": {"provider": "github", "instance": "github.com", "providerAccountId": "github.com#1"},
        "target": {"repositoryId": "repo-1", "bindingRevision": 0, "primaryRemote": "origin",
            "project": {"id": "1", "owner": "acme", "name": "app"}, "number": number},
    })
}

#[tokio::test]
async fn an_unbound_repository_is_read_with_the_hosts_current_account() {
    let (client, recorded) = fake_server(Box::new(|request| match request.path.as_str() {
        "/api/source-control/binding" => (
            200,
            json!({
                "repository": {"repositoryId": "repo-1", "configRevision": "c1", "remotes": [
                    {"name": "origin",
                     "fetch": {"displayUrl": "https://github.com/acme/app.git", "fingerprint": "f1"},
                     "push": {"displayUrl": "https://github.com/acme/app.git", "fingerprint": "f1"}}
                ]},
                "revision": 0, "binding": null,
            }),
        ),
        "/api/source-control/instances" => (
            200,
            json!({"instances": [{"provider": "github", "instance": "github.com"}]}),
        ),
        "/api/source-control/github/auth/status" => (
            200,
            json!({"provider": "github", "instance": "github.com", "status": "connected",
                "connected": true, "accounts": [
                    {"id": "old", "current": false, "status": "valid"},
                    {"id": "acct-1", "current": true, "status": "valid"}]}),
        ),
        _ => (404, json!({"error": "unexpected"})),
    }))
    .await;
    let BindingLookup::Repository(read) = client
        .source_control_binding("/work/project")
        .await
        .expect("binding")
    else {
        panic!("a repository");
    };
    let known = client.source_control_instances().await.expect("instances");
    let status = client
        .source_control_auth_status(&known[0].identity)
        .await
        .expect("status");
    assert_eq!(status.current_account.as_deref(), Some("acct-1"));
    let contexts = read_contexts(&read, "/work/project", &known, |_| {
        status.current_account.clone()
    });
    assert_eq!(contexts, vec![context()]);
    let recorded = recorded.lock().expect("log");
    assert_eq!(recorded[0].param("directory"), Some("/work/project"));
    assert_eq!(recorded[2].param("instance"), Some("github.com"));
}

#[tokio::test]
async fn a_non_repository_folder_and_a_signed_out_host_are_not_failures() {
    let (client, _) = fake_server(Box::new(|request| match request.path.as_str() {
        "/api/source-control/binding" => (
            422,
            json!({"error": "not a repository", "code": "UNSUPPORTED_SOURCE_CONTROL_REPOSITORY"}),
        ),
        _ => (
            200,
            json!({"provider": "github", "instance": "github.com", "status": "disconnected",
                "connected": false, "accounts": []}),
        ),
    }))
    .await;
    assert_eq!(
        client.source_control_binding("/tmp").await.expect("lookup"),
        BindingLookup::NotRepository
    );
    let identity = context().identity;
    let status = client
        .source_control_auth_status(&identity)
        .await
        .expect("status");
    assert_eq!(status.current_account, None);
    assert!(!status.unreachable);
}

#[tokio::test]
async fn a_list_page_carries_the_context_and_parses_each_kind() {
    let (client, recorded) = fake_server(Box::new(|request| {
        let kind = request.param("kind").unwrap_or_default();
        let items = if kind == "pull" {
            vec![pull(7)]
        } else {
            vec![issue(1, "open"), issue(2, "not_planned")]
        };
        (
            200,
            json!({"connected": true, "repo": {"owner": "acme", "repo": "app", "url": "u"},
                "items": items, "cursor": "c2", "hasMore": true, "total": 9}),
        )
    }))
    .await;
    let query = ListQuery {
        kind: RepositoryKind::Issue,
        filter: ListFilter {
            state: StateFilter::All,
            people: PeopleFilter::Created,
        },
        text: "  crash ".into(),
    };
    let Page::Items {
        items,
        cursor,
        has_more,
    } = client
        .source_control_references(&context(), &query, Some("c1"))
        .await
        .expect("page")
    else {
        panic!("items");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].labels[0].color.as_deref(), Some("d73a4a"));
    assert_eq!(items[1].state, ItemState::NotPlanned);
    assert_eq!(cursor.as_deref(), Some("c2"));
    assert!(has_more);
    let pulls = client
        .source_control_references(
            &context(),
            &ListQuery {
                kind: RepositoryKind::Pull,
                ..ListQuery::default()
            },
            None,
        )
        .await
        .expect("pulls");
    let Page::Items { items, .. } = pulls else {
        panic!("items");
    };
    assert!(items[0].upstream);
    assert_eq!(items[0].pull.as_ref().map(|pull| pull.draft), Some(true));

    let recorded = recorded.lock().expect("log");
    let first = &recorded[0];
    assert_eq!(first.method, "GET");
    assert_eq!(first.path, "/api/source-control/github/references");
    for (key, value) in [
        ("instance", "github.com"),
        ("directory", "/work/project"),
        ("repositoryId", "repo-1"),
        ("accountId", "acct-1"),
        ("bindingRevision", "0"),
        ("primaryRemote", "origin"),
        ("kind", "issue"),
        ("state", "all"),
        ("people", "created"),
        ("query", "crash"),
        ("cursor", "c1"),
    ] {
        assert_eq!(first.param(key), Some(value), "{key}");
    }
    assert_eq!(recorded[1].param("query"), None);
}

#[tokio::test]
async fn a_page_of_the_wrong_kind_is_malformed_and_disconnected_is_its_own_answer() {
    let (client, _) = fake_server(Box::new(|request| {
        if request.param("kind") == Some("pull") {
            (200, json!({"connected": false}))
        } else {
            (
                200,
                json!({"connected": true, "repo": {"owner": "a", "repo": "b", "url": "u"},
                    "items": [pull(1)], "cursor": null, "hasMore": false, "total": 1}),
            )
        }
    }))
    .await;
    let error = client
        .source_control_references(&context(), &ListQuery::default(), None)
        .await
        .expect_err("a pull in an issue list");
    assert!(matches!(
        error,
        SourceControlError::Transport(TransportError::Malformed(_))
    ));
    let disconnected = client
        .source_control_references(
            &context(),
            &ListQuery {
                kind: RepositoryKind::Pull,
                ..ListQuery::default()
            },
            None,
        )
        .await
        .expect("answer");
    assert_eq!(disconnected, Page::Disconnected);
}

#[tokio::test]
async fn a_provider_401_is_the_routes_answer_and_the_servers_own_401_locks() {
    let (client, _) = fake_server(Box::new(|request| {
        if request.path.ends_with("/references") {
            (
                401,
                json!({"error": "GitHub account is unavailable", "code": "SOURCE_CONTROL_ACCOUNT_UNAVAILABLE"}),
            )
        } else {
            (401, json!({"error": "UI authentication required", "locked": true}))
        }
    }))
    .await;
    let failures = client.authentication_failures();
    let error = client
        .source_control_references(&context(), &ListQuery::default(), None)
        .await
        .expect_err("refused");
    assert_eq!(error.code(), Some("SOURCE_CONTROL_ACCOUNT_UNAVAILABLE"));
    assert_eq!(error.message(), Some("GitHub account is unavailable"));
    assert_eq!(
        *failures.borrow(),
        0,
        "a provider refusal keeps the sign-in"
    );

    let error = client
        .source_control_providers()
        .await
        .expect_err("server lock");
    assert!(matches!(
        error,
        SourceControlError::Transport(TransportError::AuthenticationRequired)
    ));
    assert_eq!(*failures.borrow(), 1);
}

#[tokio::test]
async fn detail_and_statuses_parse_the_preview_fields() {
    let (client, recorded) = fake_server(Box::new(|request| {
        if request.path.ends_with("/references/status") {
            return (
                200,
                json!({"connected": true, "statuses": [{"owner": "acme", "repo": "app", "number": 7,
                    "checks": {"state": "failure", "total": 3, "success": 1, "failure": 2, "pending": 0},
                    "mergeable": true, "mergeableState": "clean"}]}),
            );
        }
        (
            200,
            json!({"connected": true, "detail": {"number": 7, "commentTotal": 5,
                "comments": [{"author": {"login": "rev"}, "body": "Looks good", "createdAt": "2026-01-02T00:00:00Z",
                    "url": "https://github.com/acme/app/pull/7#r1", "path": null, "line": null, "review": "approved"}],
                "pull": {"reviewDecision": "review_required", "reviewers": [], "checks": null,
                    "additions": 10, "deletions": 2, "changedFiles": 3, "commits": [], "commitTotal": null}}}),
        )
    }))
    .await;
    let Page::Items { items, .. } = page(
        serde_json::from_value(json!({"connected": true, "repo": {}, "items": [pull(7)],
            "cursor": null, "hasMore": false}))
        .expect("page body"),
        "github",
        RepositoryKind::Pull,
    )
    .expect("page") else {
        panic!("items");
    };
    let detail = client
        .source_control_reference_detail(&context(), &items[0])
        .await
        .expect("detail")
        .expect("connected");
    assert_eq!(detail.comment_total, 5);
    assert_eq!(detail.comments[0].review, Some(CommentReview::Approved));
    let pull_detail = detail.pull.expect("pull detail");
    assert_eq!(
        pull_detail.review_decision,
        Some(ReviewDecision::ReviewRequired)
    );
    assert_eq!(pull_detail.additions, 10);
    let statuses = client
        .source_control_pull_statuses(&context(), &items)
        .await
        .expect("statuses");
    assert_eq!(
        statuses[0].checks.map(|checks| checks.state),
        Some(ChecksState::Failure)
    );
    client
        .source_control_refresh_reads(&context().identity, Some(&WriteTarget::of(&items[0])))
        .await
        .expect("item refresh");
    client
        .source_control_refresh_reads(&context().identity, None)
        .await
        .expect("list refresh");
    let recorded = recorded.lock().expect("log");
    assert_eq!(recorded[0].param("owner"), Some("acme"));
    assert_eq!(recorded[0].param("number"), Some("7"));
    assert_eq!(recorded[1].param("pulls"), Some("acme/app#7"));
    assert_eq!(recorded[2].method, "POST");
    assert_eq!(
        recorded[2].path,
        "/api/source-control/github/read-cache/refresh"
    );
    assert_eq!(recorded[2].param("scope"), Some("item"));
    assert_eq!(recorded[2].param("number"), Some("7"));
    assert_eq!(recorded[3].param("scope"), Some("lists"));
    assert_eq!(recorded[3].param("owner"), None);
}

#[tokio::test]
async fn writes_send_the_exact_context_and_check_their_receipt() {
    let (client, recorded) = fake_server(Box::new(|request| match request.path.as_str() {
        "/api/source-control/github/issues/comment" => (200, receipt(3, json!({}))),
        "/api/source-control/github/pr/state" => (200, receipt(7, json!({"state": "closed"}))),
        "/api/source-control/github/pr/review" => (200, receipt(7, json!({"commented": true}))),
        // A receipt for another item is never taken as this write's.
        _ => (200, receipt(99, json!({}))),
    }))
    .await;
    let target = WriteTarget {
        owner: "acme".into(),
        repo: "app".into(),
        number: 3,
        head_sha: None,
    };
    client
        .source_control_comment(
            &context(),
            RepositoryKind::Issue,
            &target,
            "key-1",
            "Thanks",
        )
        .await
        .expect("comment");
    let pull_target = WriteTarget {
        number: 7,
        ..target.clone()
    };
    let state = client
        .source_control_set_state(
            &context(),
            RepositoryKind::Pull,
            &pull_target,
            "key-2",
            TargetState::Closed,
        )
        .await
        .expect("state");
    assert_eq!(state, TargetState::Closed);
    let review_target = WriteTarget {
        head_sha: Some("abc123".into()),
        ..pull_target
    };
    let outcome = client
        .source_control_review(
            &context(),
            &review_target,
            "key-3",
            ReviewVerdict::RequestChanges,
            Some("Please split"),
        )
        .await
        .expect("review");
    assert!(outcome.commented);
    let mismatch = client
        .source_control_comment(&context(), RepositoryKind::Pull, &target, "key-4", "x")
        .await
        .expect_err("receipt for another item");
    assert!(matches!(
        mismatch,
        SourceControlError::Transport(TransportError::Malformed(_))
    ));

    let recorded = recorded.lock().expect("log");
    assert_eq!(recorded[0].method, "POST");
    assert_eq!(recorded[0].param("instance"), Some("github.com"));
    assert_eq!(
        recorded[0].body,
        json!({
            "provider": "github", "instance": "github.com", "directory": "/work/project",
            "repositoryId": "repo-1", "accountId": "acct-1", "bindingRevision": 0,
            "primaryRemote": "origin", "idempotencyKey": "key-1",
            "target": {"project": {"owner": "acme", "name": "app"}, "number": 3},
            "body": "Thanks",
        })
    );
    assert_eq!(recorded[1].body["state"], json!("closed"));
    assert_eq!(recorded[2].body["verdict"], json!("request-changes"));
    assert_eq!(recorded[2].body["target"]["headSha"], json!("abc123"));
    assert_eq!(recorded[2].body["body"], json!("Please split"));
}

#[tokio::test]
async fn a_refused_write_keeps_the_servers_reason() {
    let (client, _) = fake_server(Box::new(|_| {
        (
            409,
            json!({"error": "Source control repository binding changed", "code": "SOURCE_CONTROL_BINDING_STALE"}),
        )
    }))
    .await;
    let error = client
        .source_control_set_state(
            &context(),
            RepositoryKind::Issue,
            &WriteTarget {
                owner: "acme".into(),
                repo: "app".into(),
                number: 1,
                head_sha: None,
            },
            "key",
            TargetState::Closed,
        )
        .await
        .expect_err("refused");
    assert!(matches!(
        error,
        SourceControlError::Refused { status: 409, .. }
    ));
    assert_eq!(error.code(), Some("SOURCE_CONTROL_BINDING_STALE"));
}
