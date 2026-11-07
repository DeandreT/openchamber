//! The composer pickers' catalogs against the scripted server: the command
//! list with each command's config read, snippets, and the folder search.

use openchamber_native_core::autocomplete::CommandScope;

use super::*;

const WORKSPACE: &str = "/work/my app";

fn client(server: &Server) -> ServerClient {
    ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("catalog-token".to_owned()),
    )
    .expect("client")
}

#[tokio::test]
async fn commands_carry_the_scope_their_config_read_found() {
    let server = Server::start(
        |request| {
            if request.target.starts_with("/prefix/api/command") {
                return Reply::json(json!({
                    "location": { "directory": WORKSPACE },
                    "data": [
                        { "name": "deploy", "description": "Listed" },
                        { "name": "goal" },
                        { "name": "broken", "description": "Kept" },
                    ],
                }));
            }
            if request
                .target
                .starts_with("/prefix/api/config/commands/deploy/config")
            {
                return Reply::json(json!({
                    "source": "md", "scope": "project", "path": "/p", "legacy": false,
                    "config": { "description": "Ship the build" },
                }));
            }
            if request
                .target
                .starts_with("/prefix/api/config/commands/goal/config")
            {
                return Reply::json(json!({
                    "source": "none", "scope": null, "path": null, "legacy": false, "config": {},
                }));
            }
            Reply::with_status(500, "application/json", "{}")
        },
        vec![],
    )
    .await;
    let commands = client(&server)
        .composer_commands(WORKSPACE)
        .await
        .expect("listed");
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].name, "deploy");
    assert_eq!(commands[0].description.as_deref(), Some("Ship the build"));
    assert_eq!(commands[0].scope, Some(CommandScope::Project));
    assert!(commands[1].from_opencode);
    assert!(!openchamber_native_core::autocomplete::offered_in_composer(
        &commands[1]
    ));
    assert_eq!(commands[2].description.as_deref(), Some("Kept"));
    assert_eq!(commands[2].scope, None);
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .all(|request| request.header("x-opencode-directory").is_some())
    );
    assert!(requests.iter().any(|request| request.target
        == "/prefix/api/config/commands/deploy/config?directory=%2Fwork%2Fmy+app"));
}

#[tokio::test]
async fn a_failed_command_list_is_an_error() {
    let server = Server::start(
        |_| Reply::with_status(500, "application/json", "{}"),
        vec![],
    )
    .await;
    assert!(client(&server).composer_commands(WORKSPACE).await.is_err());
}

#[tokio::test]
async fn snippets_mark_project_ones_and_keep_aliases() {
    let server = Server::start(
        |_| {
            Reply::json(json!([
                { "name": "sig", "content": "Thanks", "aliases": ["s"], "filePath": "/u/sig.md", "source": "global" },
                { "name": "bug", "content": "Steps", "description": "Bug report", "filePath": "/p/bug.md", "source": "project" },
            ]))
        },
        vec![],
    )
    .await;
    let snippets = client(&server)
        .composer_snippets(WORKSPACE)
        .await
        .expect("listed");
    assert_eq!(snippets[0].aliases, ["s"]);
    assert!(!snippets[0].project);
    assert!(snippets[1].project);
    assert_eq!(snippets[1].description.as_deref(), Some("Bug report"));
    assert!(
        server.requests()[0]
            .target
            .starts_with("/prefix/api/config/snippets?directory=")
    );
}
