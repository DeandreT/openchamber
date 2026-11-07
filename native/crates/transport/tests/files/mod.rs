//! Filesystem routes against the scripted authenticated HTTP server: the
//! workspace header, query fields, bodies and how each refusal reads.

use openchamber_native_core::file_tree::{EntryKind, ListingFailure};
use openchamber_native_transport::files::{ContentQuery, ContentSearch, FilesError, StatRead};

use super::*;

const WORKSPACE: &str = "/work/my app";

fn client(server: &Server) -> ServerClient {
    ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("files-token".to_owned()),
    )
    .expect("client")
}

/// The decoded query pairs of a request target.
pub(super) fn query(request: &Recorded) -> Vec<(String, String)> {
    let query = request
        .target
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    reqwest::Url::parse(&format!("http://h/?{query}"))
        .expect("a query")
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn assert_workspace(request: &Recorded) {
    assert_eq!(request.header("authorization"), Some("Bearer files-token"));
    assert_eq!(
        request.header("x-opencode-directory"),
        Some("%2Fwork%2Fmy%20app")
    );
    assert_eq!(request.header("x-opencode-directory-encoding"), Some("uri"));
}

#[tokio::test]
async fn a_listing_names_the_workspace_and_maps_entry_kinds() {
    let server = Server::start(
        |_| {
            Reply::json(json!({
                "path": "/work/my app",
                "entries": [
                    { "name": "src", "path": "/work/my app/src", "isDirectory": true, "isFile": false, "isSymbolicLink": false },
                    { "name": "link", "path": "/work/my app/link", "isDirectory": true, "isFile": false, "isSymbolicLink": true },
                    { "name": "a.txt", "path": "/work/my app/a.txt", "isDirectory": false, "isFile": true, "isSymbolicLink": false },
                ],
            }))
        },
        vec![],
    )
    .await;
    let entries = client(&server)
        .list_directory(WORKSPACE, "/work/my app", true)
        .await
        .expect("listed");
    assert_eq!(
        entries.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
        [EntryKind::Directory, EntryKind::Directory, EntryKind::File]
    );
    let requests = server.requests();
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].target.starts_with("/prefix/api/fs/list?"));
    assert_eq!(
        query(&requests[0]),
        [
            ("path".to_owned(), "/work/my app".to_owned()),
            ("respectGitignore".to_owned(), "true".to_owned())
        ]
    );
    assert_workspace(&requests[0]);
}

#[tokio::test]
async fn path_observation_distinguishes_files_folders_and_missing_paths() {
    let server = Server::start(
        |request| {
            assert!(request.target.starts_with("/prefix/api/fs/list?"));
            let parent = query(request)[0].1.clone();
            match parent.as_str() {
                "/work/my app" => Reply::json(json!({
                    "entries": [
                        { "name": "src", "path": "/work/my app/src", "isDirectory": true },
                        { "name": "a.txt", "path": "/work/my app/a.txt", "isDirectory": false }
                    ]
                })),
                "/work/my app/deleted" => Reply::with_status(
                    404,
                    "application/json",
                    r#"{"error":"Directory not found","reason":"not-found"}"#,
                ),
                "/work/my app/duplicate" => Reply::json(json!({
                    "entries": [
                        { "name": "a", "path": "/work/my app/duplicate/a", "isDirectory": true },
                        { "name": "a", "path": "/work/my app/duplicate/a", "isDirectory": false }
                    ]
                })),
                _ => Reply::with_status(500, "application/json", r#"{"error":"unavailable"}"#),
            }
        },
        vec![],
    )
    .await;
    let client = client(&server);
    assert_eq!(
        client
            .path_kind(WORKSPACE, "/work/my app/src")
            .await
            .expect("folder observed"),
        Some(EntryKind::Directory)
    );
    assert_eq!(
        client
            .path_kind(WORKSPACE, "/work/my app/a.txt")
            .await
            .expect("file observed"),
        Some(EntryKind::File)
    );
    assert_eq!(
        client
            .path_kind(WORKSPACE, "/work/my app/missing")
            .await
            .expect("missing entry"),
        None
    );
    assert_eq!(
        client
            .path_kind(WORKSPACE, "/work/my app/deleted/child")
            .await
            .expect("missing parent"),
        None
    );
    assert!(
        client
            .path_kind(WORKSPACE, "/work/my app/unavailable/child")
            .await
            .is_err()
    );
    assert!(
        client
            .path_kind(WORKSPACE, "/work/my app/duplicate/a")
            .await
            .is_err()
    );
    for request in server.requests() {
        assert_workspace(&request);
        assert!(
            !query(&request)
                .iter()
                .any(|(key, _)| key == "respectGitignore")
        );
    }
}

#[tokio::test]
async fn listing_failures_stay_failures() {
    let server = Server::start(
        |request| {
            let path = query(request)[0].1.clone();
            match path.as_str() {
                "/missing" => Reply::with_status(
                    404,
                    "application/json",
                    r#"{"error":"Directory not found","reason":"not-found"}"#,
                ),
                "/file" => Reply::with_status(
                    400,
                    "application/json",
                    r#"{"error":"Specified path is not a directory","reason":"not-directory"}"#,
                ),
                "/denied" => Reply::with_status(
                    403,
                    "application/json",
                    r#"{"error":"Access to directory denied","reason":"os-permission"}"#,
                ),
                "/auth" => Reply::with_status(401, "application/json", "{}"),
                _ => Reply::with_status(500, "application/json", r#"{"error":"disk on fire"}"#),
            }
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let failure = |path: &'static str| {
        let client = client.clone();
        async move {
            client
                .list_directory(WORKSPACE, path, false)
                .await
                .expect_err("a refusal is never an empty folder")
        }
    };
    assert!(matches!(failure("/missing").await, FilesError::NotFound));
    assert!(matches!(failure("/file").await, FilesError::NotDirectory));
    assert!(matches!(
        failure("/denied").await,
        FilesError::PermissionDenied
    ));
    assert!(matches!(
        failure("/auth").await,
        FilesError::Transport(TransportError::AuthenticationRequired)
    ));
    let other = failure("/broken").await;
    assert_eq!(other.message(), Some("disk on fire"));
    assert_eq!(
        other.into_listing_failure(),
        ListingFailure::Failed(Some("disk on fire".to_owned()))
    );
    assert!(
        query(&server.requests()[0]).len() == 1,
        "respectGitignore is sent only when set"
    );
}

#[tokio::test]
async fn name_search_reads_opencode_hits_under_the_workspace() {
    let server = Server::start(
        |_| {
            Reply::json(json!({
                "location": { "directory": "/work/my app" },
                "data": [{ "path": "src/app.txt", "type": "file" }],
            }))
        },
        vec![],
    )
    .await;
    let hits = client(&server)
        .find_files(WORKSPACE, "app", 50)
        .await
        .expect("found");
    assert_eq!(hits[0].path, "/work/my app/src/app.txt");
    assert_eq!(hits[0].relative_path, "src/app.txt");
    let request = &server.requests()[0];
    assert!(request.target.starts_with("/prefix/api/fs/find?"));
    assert_eq!(
        query(request),
        [
            ("query".to_owned(), "app".to_owned()),
            ("type".to_owned(), "file".to_owned()),
            ("limit".to_owned(), "50".to_owned())
        ]
    );
    assert_workspace(request);
}

#[tokio::test]
async fn folder_search_asks_for_directories() {
    let server = Server::start(
        |_| {
            Reply::json(json!({
                "location": { "directory": "/work/my app" },
                "data": [{ "path": "src/", "type": "directory" }],
            }))
        },
        vec![],
    )
    .await;
    let hits = client(&server)
        .find_folders(WORKSPACE, "src", 20)
        .await
        .expect("found");
    assert_eq!(hits[0].relative_path, "src/");
    assert_eq!(
        query(&server.requests()[0]),
        [
            ("query".to_owned(), "src".to_owned()),
            ("type".to_owned(), "directory".to_owned()),
            ("limit".to_owned(), "20".to_owned())
        ]
    );
}

#[tokio::test]
async fn content_search_reports_matches_invalid_patterns_and_a_missing_ripgrep() {
    let server = Server::start(
        |request| {
            let pairs = query(request);
            match pairs[0].1.as_str() {
                "(" => Reply::with_status(
                    400,
                    "application/json",
                    r#"{"error":"unclosed group","reason":"invalid-pattern"}"#,
                ),
                "none" => Reply::with_status(
                    501,
                    "application/json",
                    r#"{"error":"ripgrep is not available","reason":"search-unavailable"}"#,
                ),
                _ => Reply::json(json!({
                    "matches": [{ "path": "/work/my app/a.txt", "relativePath": "a.txt", "line": 3, "column": 2,
                                  "text": "line 1", "ranges": [[0, 6]], "cutStart": false }],
                    "stopped": "limit",
                })),
            }
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let search = |text: &str, regex: bool| ContentQuery {
        query: text.to_owned(),
        regex,
        limit: 9_999,
        include_hidden: true,
        respect_gitignore: false,
    };
    let ContentSearch::Matches { matches, stopped } = client
        .search_contents(WORKSPACE, &search("line 1", false))
        .await
        .expect("searched")
    else {
        panic!("matches");
    };
    assert_eq!((matches[0].line, matches[0].ranges[0]), (3, (0, 6)));
    assert_eq!(
        stopped,
        Some(openchamber_native_transport::files::SearchStop::Limit)
    );
    assert_eq!(
        client
            .search_contents(WORKSPACE, &search("(", true))
            .await
            .expect("declared"),
        ContentSearch::InvalidPattern("unclosed group".to_owned())
    );
    assert_eq!(
        client
            .search_contents(WORKSPACE, &search("none", false))
            .await
            .expect("declared"),
        ContentSearch::Unavailable
    );
    let request = &server.requests()[0];
    assert_eq!(
        query(request),
        [
            ("query".to_owned(), "line 1".to_owned()),
            ("regex".to_owned(), "false".to_owned()),
            ("limit".to_owned(), "500".to_owned()),
            ("includeHidden".to_owned(), "true".to_owned()),
            ("respectGitignore".to_owned(), "false".to_owned())
        ]
    );
    assert_workspace(request);
}

#[tokio::test]
async fn stat_read_and_write_round_trip_text() {
    let server = Server::start(
        |request| {
            if request.target.contains("/api/fs/stat") {
                if request.target.contains("gone") {
                    return Reply::json(json!({ "path": "/work/my app/gone", "exists": false }));
                }
                return Reply::json(
                    json!({ "path": "/real/a.txt", "isFile": true, "size": 12, "mtimeMs": 1.5 }),
                );
            }
            if request.target.contains("/api/fs/read") {
                if request.target.contains("json") {
                    return Reply::json(json!({ "unexpected": true }));
                }
                return Reply::with_status(200, "text/plain; charset=utf-8", "first\r\n世界\r\n");
            }
            Reply::json(json!({ "success": true, "path": "/work/my app/a.txt" }))
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let StatRead::Found { path, stat } = client
        .stat_file(WORKSPACE, "/work/my app/a.txt", false)
        .await
        .expect("stat")
    else {
        panic!("found");
    };
    assert_eq!(
        (path.as_str(), stat.size, stat.mtime_ms),
        ("/real/a.txt", 12, Some(1.5))
    );
    assert_eq!(
        client
            .stat_file(WORKSPACE, "/work/my app/gone", false)
            .await
            .expect("stat"),
        StatRead::Missing
    );
    assert_eq!(
        client
            .read_file(WORKSPACE, "/etc/hostname", true)
            .await
            .expect("read"),
        "first\r\n世界\r\n"
    );
    assert!(matches!(
        client.read_file(WORKSPACE, "/work/json", false).await,
        Err(FilesError::Transport(TransportError::Malformed(_)))
    ));
    client
        .write_file(WORKSPACE, "/work/my app/a.txt", "x\r\ny")
        .await
        .expect("written");
    let requests = server.requests();
    assert_eq!(
        query(&requests[0]),
        [
            ("path".to_owned(), "/work/my app/a.txt".to_owned()),
            ("optional".to_owned(), "true".to_owned())
        ]
    );
    assert_eq!(
        query(&requests[2]),
        [
            ("path".to_owned(), "/etc/hostname".to_owned()),
            ("allowOutsideWorkspace".to_owned(), "true".to_owned())
        ]
    );
    let write = requests.last().expect("the write");
    assert_eq!(write.method, "POST");
    assert_eq!(write.target, "/prefix/api/fs/write");
    assert_eq!(
        write.json(),
        json!({ "path": "/work/my app/a.txt", "content": "x\r\ny" })
    );
    for request in &requests {
        assert_workspace(request);
    }
}

#[tokio::test]
async fn a_write_outside_the_workspace_and_a_rename_onto_a_file_are_refused() {
    let server = Server::start(
        |request| {
            if request.target.ends_with("/api/fs/rename") {
                return Reply::with_status(
                    409,
                    "application/json",
                    r#"{"error":"Destination path already exists","reason":"already-exists"}"#,
                );
            }
            Reply::with_status(
                400,
                "application/json",
                r#"{"error":"Path is outside of active workspace"}"#,
            )
        },
        vec![],
    )
    .await;
    let client = client(&server);
    let refused = client
        .write_file(WORKSPACE, "/tmp/x", "x")
        .await
        .expect_err("refused");
    assert_eq!(
        refused.message(),
        Some("Path is outside of active workspace")
    );
    assert!(matches!(
        client
            .rename_path(WORKSPACE, "/work/my app/a", "/work/my app/b")
            .await,
        Err(FilesError::AlreadyExists)
    ));
    assert_eq!(
        server.requests()[1].json(),
        json!({ "oldPath": "/work/my app/a", "newPath": "/work/my app/b" })
    );
}
