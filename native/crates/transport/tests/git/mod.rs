//! Git routes against the scripted authenticated HTTP server: the directory
//! query, mutation bodies, declared refusals and the planned network
//! operation lifecycle.

use openchamber_native_core::git_changes::{
    ChangesRead, GitChanges, NetworkAction, OperationState, RepositoryBinding, TransportMode,
    network_request,
};
use openchamber_native_transport::git::{GitError, PathDiff};

use super::files::query;
use super::*;

const REPOSITORY: &str = "/work/repo";

fn client(server: &Server) -> ServerClient {
    ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("git-token".to_owned()),
    )
    .expect("client")
}

fn directory(request: &Recorded) -> Option<String> {
    query(request)
        .into_iter()
        .find(|(key, _)| key == "directory")
        .map(|(_, value)| value)
}

fn success() -> Reply {
    Reply::json(json!({ "success": true }))
}

#[tokio::test]
async fn index_mutations_send_their_paths_to_the_repository() {
    let server = Server::start(|_| success(), vec![]).await;
    let client = client(&server);
    let paths = vec!["a.txt".to_owned(), "dir/b c.txt".to_owned()];
    client.stage(REPOSITORY, &paths).await.expect("staged");
    client
        .unstage(REPOSITORY, &paths[..1])
        .await
        .expect("unstaged");
    client
        .discard(REPOSITORY, "a.txt")
        .await
        .expect("discarded");
    let requests = server.requests();
    let routes: Vec<(&str, &str)> = requests
        .iter()
        .map(|request| {
            (
                request.method.as_str(),
                request.target.split('?').next().expect("a path"),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            ("POST", "/prefix/api/git/stage"),
            ("POST", "/prefix/api/git/unstage"),
            ("POST", "/prefix/api/git/revert")
        ]
    );
    assert_eq!(
        requests[0].json(),
        json!({ "paths": ["a.txt", "dir/b c.txt"] })
    );
    assert_eq!(requests[1].json(), json!({ "paths": ["a.txt"] }));
    assert_eq!(
        requests[2].json(),
        json!({ "path": "a.txt", "scope": "working" }),
        "a row discard keeps the staged part"
    );
    for request in &requests {
        assert_eq!(directory(request).as_deref(), Some(REPOSITORY));
        assert_eq!(request.header("authorization"), Some("Bearer git-token"));
    }
}

#[tokio::test]
async fn a_failed_mutation_carries_the_servers_reason() {
    let server = Server::start(
        |_| {
            Reply::with_status(
                500,
                "application/json",
                r#"{"error":"Unable to create '/work/repo/.git/index.lock': File exists."}"#,
            )
        },
        vec![],
    )
    .await;
    let error = client(&server)
        .stage(REPOSITORY, &["a".to_owned()])
        .await
        .expect_err("refused");
    assert!(matches!(error, GitError::Refused { status: 500, .. }));
    assert!(
        error
            .message()
            .is_some_and(|message| message.contains("index.lock"))
    );
}

#[tokio::test]
async fn status_and_diffs_read_every_declared_outcome() {
    let server = Server::start(
        |request| {
            if request.target.contains("/api/git/status") {
                return Reply::json(json!({
                    "current": "main", "tracking": "origin/main", "ahead": 0, "behind": 2, "unborn": false,
                    "files": [{ "path": "README.md", "index": " ", "working_dir": "M" }],
                    "isClean": false, "diffStats": { "staged": {}, "working": { "README.md": { "insertions": 1, "deletions": 0 } } },
                    "mergeInProgress": null, "rebaseInProgress": null,
                }));
            }
            let path = query(request)
                .into_iter()
                .find(|(key, _)| key == "path")
                .map(|(_, value)| value)
                .unwrap_or_default();
            match path.as_str() {
                "nested/" => Reply::with_status(
                    422,
                    "application/json",
                    r#"{"error":"Path is a nested repository","code":"nested_repository"}"#,
                ),
                "notes/" => Reply::with_status(
                    422,
                    "application/json",
                    r#"{"error":"Path is a directory of untracked files","code":"untracked_directory"}"#,
                ),
                "gone" => Reply::with_status(
                    404,
                    "application/json",
                    r#"{"error":"not found","code":"path_not_found"}"#,
                ),
                _ => Reply::json(json!({ "diff": "diff --git a/README.md b/README.md\n", "submodule": null })),
            }
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let ChangesRead::Repository(changes) = client.git_changes(REPOSITORY).await.expect("status")
    else {
        panic!("a repository");
    };
    assert_eq!(changes.behind, 2);
    assert_eq!(changes.unstaged()[0].path, "README.md");
    assert_eq!(
        client
            .git_diff(REPOSITORY, "README.md", true)
            .await
            .expect("diff"),
        PathDiff::Patch("diff --git a/README.md b/README.md\n".to_owned())
    );
    assert_eq!(
        client
            .git_diff(REPOSITORY, "nested/", false)
            .await
            .expect("declared"),
        PathDiff::NestedRepository
    );
    assert_eq!(
        client
            .git_diff(REPOSITORY, "notes/", false)
            .await
            .expect("declared"),
        PathDiff::UntrackedDirectory
    );
    assert_eq!(
        client
            .git_diff(REPOSITORY, "gone", false)
            .await
            .expect("declared"),
        PathDiff::NotFound
    );
    let requests = server.requests();
    assert_eq!(
        query(&requests[0]),
        [("directory".to_owned(), REPOSITORY.to_owned())],
        "the full status, with line counts"
    );
    assert_eq!(
        query(&requests[1]),
        [
            ("directory".to_owned(), REPOSITORY.to_owned()),
            ("path".to_owned(), "README.md".to_owned()),
            ("context".to_owned(), "3".to_owned()),
            ("staged".to_owned(), "true".to_owned())
        ]
    );
}

#[tokio::test]
async fn commits_and_branch_operations_send_the_documented_bodies() {
    let server = Server::start(
        |request| {
            if request.target.contains("/api/git/commit") {
                return Reply::json(
                    json!({ "success": true, "commit": "abc123", "branch": "main",
                    "summary": { "changes": 1, "insertions": 1, "deletions": 0 } }),
                );
            }
            if request.method == "GET" {
                return Reply::json(json!({
                    "all": ["main", "feature", "remotes/origin/main"],
                    "current": "main",
                    "branches": {},
                }));
            }
            success()
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let created = client
        .commit(REPOSITORY, "Message")
        .await
        .expect("committed");
    assert_eq!((created.commit.as_str(), created.empty), ("abc123", false));
    let branches = client.branches(REPOSITORY).await.expect("branches");
    assert_eq!(branches.local, ["main", "feature"]);
    assert_eq!(branches.remote, ["origin/main"]);
    assert_eq!(branches.current.as_deref(), Some("main"));
    client
        .create_branch(REPOSITORY, "topic")
        .await
        .expect("created");
    client
        .checkout_branch(REPOSITORY, "topic")
        .await
        .expect("checked out");
    client
        .rename_branch(REPOSITORY, "topic", "topic-2")
        .await
        .expect("renamed");
    client
        .delete_branch(REPOSITORY, "topic-2")
        .await
        .expect("deleted");
    let requests = server.requests();
    let bodies: Vec<(&str, &str, Value)> = requests
        .iter()
        .filter(|request| request.method != "GET")
        .map(|request| {
            (
                request.method.as_str(),
                request.target.split('?').next().expect("a path"),
                request.json(),
            )
        })
        .collect();
    assert_eq!(
        bodies,
        [
            (
                "POST",
                "/prefix/api/git/commit",
                json!({ "message": "Message", "addAll": false })
            ),
            (
                "POST",
                "/prefix/api/git/branches",
                json!({ "name": "topic" })
            ),
            (
                "POST",
                "/prefix/api/git/checkout",
                json!({ "branch": "topic" })
            ),
            (
                "PUT",
                "/prefix/api/git/branches/rename",
                json!({ "oldName": "topic", "newName": "topic-2" })
            ),
            (
                "DELETE",
                "/prefix/api/git/branches",
                json!({ "branch": "topic-2" })
            ),
        ]
    );
}

fn operation_body(id: &str, state: &str, steps: &[&str]) -> Value {
    json!({
        "operationId": id,
        "runtimeIdentity": { "id": "server_1", "platform": "web" },
        "transport": { "mode": "system", "verification": { "status": "unverified", "reason": "system-credentials" } },
        "target": { "operation": "push", "repositoryId": "repo_1", "bindingRevision": 0, "configRevision": "c",
                    "remote": { "name": "origin", "endpoint": { "displayUrl": "http://127.0.0.1:1/r.git", "fingerprint": "f" } },
                    "sourceRef": "refs/heads/main", "destinationRef": "refs/heads/main" },
        "completedSteps": steps,
        "state": state,
    })
}

#[tokio::test]
async fn a_push_is_planned_executed_read_and_cancelled_by_its_id() {
    let server = Server::start(
        |request| {
            let path = request.target.split('?').next().expect("a path");
            if path.ends_with("/api/source-control/binding") {
                return Reply::json(json!({
                    "repository": { "supported": true, "repositoryId": "repo_1", "configRevision": "c", "bare": false,
                                    "remotes": [{ "name": "origin",
                                                  "fetch": { "displayUrl": "http://127.0.0.1:1/r.git", "fingerprint": "f" },
                                                  "push": { "displayUrl": "http://127.0.0.1:1/r.git", "fingerprint": "f" } }] },
                    "revision": 0,
                    "binding": null,
                }));
            }
            if path.ends_with("/api/git/network-operations") {
                let mut reply = Reply::json(operation_body("git_1", "planned", &[]));
                reply.status = 201;
                return reply;
            }
            if path.ends_with("/execute") {
                return Reply::json(operation_body("git_1", "succeeded", &["validated", "transferred"]));
            }
            if path.ends_with("/cancel") {
                return Reply::json(operation_body("git_1", "outcome-unknown", &["validated"]));
            }
            Reply::json(operation_body("git_1", "running", &["validated"]))
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let binding: RepositoryBinding = client
        .repository_binding(REPOSITORY)
        .await
        .expect("binding");
    assert_eq!(binding.remotes[0].mode, TransportMode::System);
    let status = GitChanges {
        branch: Some("main".to_owned()),
        tracking: Some("origin/main".to_owned()),
        ..GitChanges::default()
    };
    let request =
        network_request(NetworkAction::Push, REPOSITORY, &binding, &status).expect("plan");
    let planned = client.plan_operation(&request).await.expect("planned");
    assert_eq!(
        (planned.id.as_str(), planned.state),
        ("git_1", OperationState::Planned)
    );
    let done = client
        .execute_operation(&planned.id)
        .await
        .expect("executed");
    assert_eq!(done.state, OperationState::Succeeded);
    assert_eq!(
        client
            .read_operation(&planned.id)
            .await
            .expect("read")
            .state,
        OperationState::Running
    );
    assert_eq!(
        client
            .cancel_operation(&planned.id)
            .await
            .expect("cancelled")
            .state,
        OperationState::OutcomeUnknown
    );
    let requests = server.requests();
    let routes: Vec<(&str, &str)> = requests
        .iter()
        .map(|request| {
            (
                request.method.as_str(),
                request.target.split('?').next().expect("a path"),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            ("GET", "/prefix/api/source-control/binding"),
            ("POST", "/prefix/api/git/network-operations"),
            ("POST", "/prefix/api/git/network-operations/git_1/execute"),
            ("GET", "/prefix/api/git/network-operations/git_1"),
            ("POST", "/prefix/api/git/network-operations/git_1/cancel"),
        ]
    );
    assert_eq!(directory(&requests[0]).as_deref(), Some(REPOSITORY));
    assert_eq!(
        requests[1].json(),
        json!({
            "operation": "push", "directory": REPOSITORY, "repositoryId": "repo_1",
            "bindingRevision": 0, "configRevision": "c",
            "remote": { "name": "origin", "endpoint": { "displayUrl": "http://127.0.0.1:1/r.git", "fingerprint": "f" } },
            "sourceRef": "refs/heads/main", "destinationRef": "refs/heads/main", "transportMode": "system",
        })
    );
    for request in &requests[2..] {
        assert!(
            request.body.is_empty(),
            "execute, read and cancel send no body"
        );
    }
    assert!(
        !requests
            .iter()
            .any(|request| request.target.contains("/api/git/push")),
        "the legacy route is never called"
    );
}

#[tokio::test]
async fn an_unknown_operation_state_is_malformed() {
    let server = Server::start(
        |_| Reply::json(operation_body("git_1", "teleported", &[])),
        vec![],
    )
    .await;
    assert!(matches!(
        client(&server).read_operation("git_1").await,
        Err(GitError::Transport(TransportError::Malformed(_)))
    ));
}
