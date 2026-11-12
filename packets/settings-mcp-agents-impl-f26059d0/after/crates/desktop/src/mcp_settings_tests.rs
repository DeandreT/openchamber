use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, TestAppContext};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::client::{ServerClient, ServerSettings};
use openchamber_native_transport::credential::Credential;
use serde_json::{Value, json};

use super::*;
use crate::connection::{Connection, ServerConfig};
use crate::settings_controls::tests::{ResponseGate, retarget};
use crate::settings_test_server::{Request, Server, settings, settle};

const DIRECTORY: &str = "/repo";

/// An MCP config store with OpenCode's status, connect and sign-in routes.
/// `fail_list` and `fail_writes` answer 500 while set.
struct Fixture {
    server: Server,
    configs: Arc<Mutex<Vec<Value>>>,
    statuses: Arc<Mutex<Vec<(String, Value)>>>,
    policies: Arc<Mutex<Value>>,
    fail_list: Arc<AtomicBool>,
    fail_writes: Arc<AtomicBool>,
}

fn fixture() -> Fixture {
    gated_fixture(None)
}

fn set_status(statuses: &Mutex<Vec<(String, Value)>>, name: &str, status: Value) {
    let mut statuses = statuses.lock().expect("statuses");
    match statuses.iter_mut().find(|(server, _)| server == name) {
        Some(entry) => entry.1 = status,
        None => statuses.push((name.to_owned(), status)),
    }
}

/// `hold` names a method whose first config write waits on the gate.
fn gated_fixture(hold: Option<(&'static str, Arc<ResponseGate>)>) -> Fixture {
    let held_once = AtomicBool::new(false);
    let configs = Arc::new(Mutex::new(vec![
        json!({"name": "files", "type": "local", "command": ["npx", "files"], "scope": "user"}),
        json!({"name": "hosted", "type": "remote", "url": "https://mcp.test", "scope": "user"}),
    ]));
    let statuses = Arc::new(Mutex::new(vec![
        ("files".to_owned(), json!({"status": "connected"})),
        (
            "hosted".to_owned(),
            json!({"status": "needs_auth", "error": "Sign in to hosted"}),
        ),
        ("console".to_owned(), json!({"status": "connected"})),
    ]));
    let policies = Arc::new(Mutex::new(json!([])));
    let fail_list = Arc::new(AtomicBool::new(false));
    let fail_writes = Arc::new(AtomicBool::new(false));
    let (stored, running, decided, list_fails, write_fails) = (
        Arc::clone(&configs),
        Arc::clone(&statuses),
        Arc::clone(&policies),
        Arc::clone(&fail_list),
        Arc::clone(&fail_writes),
    );
    let server = Server::new(move |request: &Request| {
        let path = request
            .target
            .split('?')
            .next()
            .unwrap_or_default()
            .to_owned();
        if request.is("GET", "/api/config/mcp") {
            if list_fails.load(Ordering::Acquire) {
                return (500, json!({"error": "fixture read failure"}));
            }
            return (200, Value::Array(stored.lock().expect("configs").clone()));
        }
        if request.is("GET", "/api/mcp") {
            let data: Vec<Value> = running
                .lock()
                .expect("statuses")
                .iter()
                .map(|(name, status)| json!({"name": name, "status": status}))
                .collect();
            return (200, json!({"location": {}, "data": data}));
        }
        if request.is("GET", "/api/config") {
            return (200, decided.lock().expect("policies").clone());
        }
        if request.is("GET", "/api/integration") {
            return (
                200,
                json!({"location": {}, "data": [{
                    "id": "mcp_hosted", "name": "hosted", "metadata": {"source": "mcp"},
                    "methods": [{"type": "oauth", "id": "browser"}],
                }]}),
            );
        }
        if let Some(rest) = path.strip_prefix("/api/experimental/mcp/") {
            let (name, operation) = rest.split_once('/').unwrap_or((rest, ""));
            let status = if operation == "connect" {
                json!({"status": "connected"})
            } else {
                json!({"status": "disabled"})
            };
            set_status(&running, name, status);
            return (200, Value::Null);
        }
        if let Some(rest) = path.strip_prefix("/api/integration/mcp_hosted/connect/oauth") {
            return match (request.method.as_str(), rest) {
                ("POST", "") => (
                    200,
                    json!({"location": {}, "data": {
                        "attemptID": "attempt", "mode": "code",
                        "url": "https://auth.test/start", "instructions": "Paste the code",
                    }}),
                ),
                ("GET", _) => (200, json!({"location": {}, "data": {"status": "pending"}})),
                _ => (200, Value::Null),
            };
        }
        let Some(name) = path.strip_prefix("/api/config/mcp/") else {
            return (404, json!({"error": "not in this fixture"}));
        };
        if let Some((method, gate)) = &hold
            && request.method == *method
            && !held_once.swap(true, Ordering::AcqRel)
        {
            gate.hold();
        }
        if write_fails.load(Ordering::Acquire) {
            return (500, json!({"error": "fixture write failure"}));
        }
        let mut stored = stored.lock().expect("configs");
        match request.method.as_str() {
            "POST" => {
                let mut config = request.body.clone();
                config["name"] = json!(name);
                stored.push(config);
                (
                    200,
                    json!({"success": true, "message": format!("MCP server \"{name}\" created.")}),
                )
            }
            "PATCH" => {
                let Some(config) = stored.iter_mut().find(|config| config["name"] == name) else {
                    return (404, json!({"error": "not found"}));
                };
                if let Some(changes) = request.body.as_object() {
                    for (key, value) in changes {
                        config[key.as_str()] = value.clone();
                    }
                }
                (200, json!({"success": true}))
            }
            "DELETE" => {
                stored.retain(|config| config["name"] != name);
                (200, json!({"success": true}))
            }
            _ => (404, json!({"error": "not in this fixture"})),
        }
    });
    Fixture {
        server,
        configs,
        statuses,
        policies,
        fail_list,
        fail_writes,
    }
}

/// A workspace connected to `server` whose draft runs in [`DIRECTORY`], with
/// Settings open on the MCP page.
fn open(
    server: &Server,
    cx: &mut TestAppContext,
) -> (AnyWindowHandle, Entity<Workspace>, Entity<McpPage>) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let mut connection =
                Connection::new(ServerConfig::for_endpoint(server.endpoint.clone()))
                    .expect("network");
            connection.client = Some(
                ServerClient::new(server.endpoint.clone(), Credential::None)
                    .expect("fixture client"),
            );
            workspace.connection = Some(connection);
            workspace.send_catalog.settings = Some(ServerSettings::default());
            workspace
                .draft_projects
                .insert(workspace.runtime.clone(), DIRECTORY.to_owned());
            workspace.show_settings(Some(Page::Mcp), window, cx);
        });
    })
    .expect("test window");
    settle(window, cx);
    let settings = settings(&workspace, cx);
    let page = cx.read(|cx| settings.read(cx).mcp.clone());
    (window, workspace, page)
}

fn act(
    window: AnyWindowHandle,
    page: &Entity<McpPage>,
    cx: &mut TestAppContext,
    action: impl FnOnce(&mut McpPage, &mut Window, &mut Context<McpPage>),
) {
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| action(page, window, cx));
    })
    .expect("test window");
    settle(window, cx);
}

fn set_url(page: &mut McpPage, text: &str, window: &mut Window, cx: &mut Context<McpPage>) {
    let url = page.editor.as_ref().expect("editor").url.clone();
    let text = text.to_owned();
    url.update(cx, |input, cx| input.set_value(text, window, cx));
}

fn set_command(page: &mut McpPage, text: &str, window: &mut Window, cx: &mut Context<McpPage>) {
    let command = page.editor.as_ref().expect("editor").command.clone();
    let text = text.to_owned();
    command.update(cx, |input, cx| input.set_value(text, window, cx));
}

/// The config writes the fixture received, connects and sign-ins left out.
fn config_writes(fixture: &Fixture) -> Vec<Request> {
    fixture
        .server
        .writes()
        .into_iter()
        .filter(|request| request.target.starts_with("/api/config/mcp/"))
        .collect()
}

fn card_status(workspace: &Entity<Workspace>, name: &str, cx: &TestAppContext) -> McpConnection {
    cx.read(|cx| {
        workspace
            .read(cx)
            .mcp_status
            .store
            .servers(DIRECTORY)
            .iter()
            .find(|server| server.name == name)
            .map(|server| server.connection)
            .expect("the work status card holds the server")
    })
}

#[test]
fn text_helpers_follow_the_reference() {
    assert_eq!(
        parse_shell_command("npx -y 'my server' \"a b\"  tail"),
        ["npx", "-y", "my server", "a b", "tail"]
    );
    assert!(is_bare_link("HTTPS://mcp.test/sse"));
    assert!(!is_bare_link("https://mcp.test and more"));
    assert!(!is_bare_link("npx"));
    assert!(has_scheme("stdio+x://thing"));
    assert!(!has_scheme("npx -y server"));
    assert_eq!(server_name("My Server_1!"), "my-server_1-");
    assert_eq!(Pairs::Environment.key("api key-1"), "API_KEY_1");
    assert_eq!(Pairs::Headers.key("  X-Api Key"), "X-Api Key");
    assert_eq!(
        parse_dot_env("# note\nA=1\n B = \"two words\" \nbroken\n=none\nC='x'"),
        [
            ("A".to_owned(), "1".to_owned()),
            ("B".to_owned(), "two words".to_owned()),
            ("C".to_owned(), "x".to_owned()),
        ]
    );
    assert_eq!(duration_label("5000").as_deref(), Some("5s"));
    assert_eq!(duration_label("1500").as_deref(), Some("1500ms"));
    assert_eq!(duration_label("0"), None);
    assert!(is_absolute_http_url("https://auth.test/meta"));
    assert!(!is_absolute_http_url("auth.test/meta"));
    assert!(!is_absolute_http_url("ftp://auth.test"));
}

#[test]
fn a_form_is_refused_before_any_request_and_carries_stored_oauth() {
    let local = Form {
        enabled: true,
        ..Form::default()
    };
    assert_eq!(
        local.refusal(),
        Some("settings.mcp.page.toast.localCommandRequired")
    );
    let remote = Form {
        kind: McpKind::Remote,
        url: " https://mcp.test ".into(),
        metadata_url: "not a link".into(),
        ..local.clone()
    };
    assert_eq!(
        remote.refusal(),
        Some("settings.mcp.page.advanced.oauthMetadataUrlInvalid")
    );
    let stored = McpServerConfig {
        name: "hosted".into(),
        kind: McpKind::Remote,
        url: "https://mcp.test".into(),
        oauth: McpOAuthSetting::Configured(
            openchamber_native_transport::mcp_config::McpOAuth {
                client_id: Some("client".into()),
                callback_port: Some(9000),
                auth_server_metadata_url: Some("https://auth.test/meta".into()),
                ..Default::default()
            },
        ),
        ..McpServerConfig::default()
    };
    let form = Form::of(&stored);
    assert_eq!(form.refusal(), None);
    let draft = form.draft(&Carried::of(&stored.oauth), McpScope::User);
    assert_eq!(draft.oauth_client_id, "client");
    assert_eq!(draft.oauth_callback_port, "9000");
    assert_eq!(
        draft.oauth_auth_server_metadata_url,
        "https://auth.test/meta"
    );
    assert!(!Carried::of(&McpOAuthSetting::Disabled).enabled);
}

#[gpui_kit::test]
fn the_list_loads_with_status_and_feeds_the_work_status_card(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, workspace, page) = open(&fixture.server, cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("mcp-open-files").is_some());
        assert!(window.try_find("mcp-open-hosted").is_some());
        assert!(
            window.try_find("mcp-runtime-console").is_some(),
            "a server no config file defines is listed read-only"
        );
        assert!(window.try_find("mcp-open-console").is_none());
        assert!(window.try_find("mcp-load-failed").is_none());
    })
    .expect("test window");
    assert_eq!(
        card_status(&workspace, "hosted", cx),
        McpConnection::NeedsAuth,
        "the page's read lands in the store the card shows"
    );
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.servers().len(), 2);
        assert_eq!(
            page.reasons.get("hosted").map(String::as_str),
            Some("Sign in to hosted")
        );
        assert_eq!(
            page.standing("files", cx),
            Some(Standing::Runtime(McpConnection::Connected))
        );
    });
    assert!(
        fixture.server.writes().is_empty(),
        "opening the page writes nothing"
    );
}

#[gpui_kit::test]
fn a_failed_list_read_offers_retry_and_never_an_empty_list(cx: &mut TestAppContext) {
    let fixture = fixture();
    fixture.fail_list.store(true, Ordering::Release);
    let (window, _, page) = open(&fixture.server, cx);
    cx.read(|cx| assert_eq!(page.read(cx).list, Load::Failed));
    act(window, &page, cx, |page, window, cx| {
        page.create_new(window, cx)
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("mcp-load-failed-retry").is_some());
        assert!(
            window.try_find("mcp-empty").is_none(),
            "a failed read is not an empty list"
        );
        assert!(
            window.try_find("mcp-editor").is_none(),
            "no draft without a list"
        );
    })
    .expect("test window");
    fixture.fail_list.store(false, Ordering::Release);
    act(window, &page, cx, |page, _, cx| page.refresh(cx));
    cx.read(|cx| assert_eq!(page.read(cx).servers().len(), 2));
}

#[gpui_kit::test]
fn an_edit_autosaves_only_when_the_form_differs(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    act(window, &page, cx, |page, window, cx| {
        page.request_save(window, cx)
    });
    assert!(
        config_writes(&fixture).is_empty(),
        "an unchanged form writes nothing"
    );
    act(window, &page, cx, |page, window, cx| {
        set_url(page, "https://mcp.test/v2", window, cx);
        page.add_pair(Pairs::Headers, "X-Team", "core", window, cx);
        // A row without a key says nothing.
        page.add_pair(Pairs::Headers, "", "ignored", window, cx);
        page.request_save(window, cx);
    });
    let sent = config_writes(&fixture);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].is("PATCH", "/api/config/mcp/hosted"));
    assert_eq!(sent[0].body["type"], "remote");
    assert_eq!(sent[0].body["url"], "https://mcp.test/v2");
    assert_eq!(sent[0].body["headers"], json!({"X-Team": "core"}));
    assert_eq!(sent[0].body["disabled"], false);
    assert_eq!(
        sent[0].body["oauth"],
        json!({}),
        "OAuth left on stays on through a save"
    );
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(!page.has_failed_writes());
        assert_eq!(
            page.editor.as_ref().expect("editor").saved.url,
            "https://mcp.test/v2",
            "the written form is the new baseline"
        );
    });
    act(window, &page, cx, |page, window, cx| {
        page.request_save(window, cx)
    });
    assert_eq!(
        config_writes(&fixture).len(),
        1,
        "the saved form is not sent again"
    );
    act(window, &page, cx, |page, window, cx| {
        page.set_enabled(false, window, cx)
    });
    assert_eq!(config_writes(&fixture)[1].body["disabled"], true);
}

#[gpui_kit::test]
fn a_failed_save_keeps_the_edit_and_retry_sends_it(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("files", window, cx)
    });
    fixture.fail_writes.store(true, Ordering::Release);
    act(window, &page, cx, |page, window, cx| {
        set_command(page, "npx\nkept", window, cx);
        page.request_save(window, cx);
    });
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(page.has_failed_writes());
        assert_eq!(page.form(cx).expect("form").command, ["npx", "kept"]);
        assert_eq!(
            page.editor.as_ref().expect("editor").saved.command,
            ["npx", "files"],
            "a failed write does not move the baseline"
        );
        assert_eq!(page.notice.as_deref(), Some("Failed to save"));
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("mcp-save-failed-retry").is_some());
    })
    .expect("test window");
    fixture.fail_writes.store(false, Ordering::Release);
    act(window, &page, cx, |page, window, cx| {
        page.request_save(window, cx)
    });
    assert_eq!(
        fixture.configs.lock().expect("configs")[0]["command"],
        json!(["npx", "kept"])
    );
    cx.read(|cx| assert!(!page.read(cx).has_failed_writes()));
}

#[gpui_kit::test]
fn an_emptied_command_is_refused_before_any_request(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("files", window, cx)
    });
    act(window, &page, cx, |page, window, cx| {
        set_command(page, "  \n", window, cx);
        page.request_save(window, cx);
    });
    assert!(config_writes(&fixture).is_empty());
    cx.read(|cx| {
        assert_eq!(
            page.read(cx).notice.as_deref(),
            Some("Command cannot be empty for a local server")
        );
    });
}

#[gpui_kit::test]
fn pasted_text_decides_the_connection_kind(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.create_new(window, cx)
    });
    act(window, &page, cx, |page, window, cx| {
        set_command(page, "https://mcp.test/sse", window, cx);
    });
    cx.read(|cx| {
        let form = page.read(cx).form(cx).expect("form");
        assert_eq!(form.kind, McpKind::Remote, "a lone link is a hosted server");
        assert_eq!(form.url, "https://mcp.test/sse");
        assert!(form.command.is_empty());
    });
    act(window, &page, cx, |page, window, cx| {
        set_url(page, "npx -y 'my server'", window, cx);
    });
    cx.read(|cx| {
        let form = page.read(cx).form(cx).expect("form");
        assert_eq!(form.kind, McpKind::Local, "a command is still a command");
        assert_eq!(form.command, ["npx", "-y", "my server"]);
        assert_eq!(form.url, "");
    });
}

fn patched_urls(fixture: &Fixture) -> Vec<String> {
    config_writes(fixture)
        .iter()
        .filter(|write| write.method == "PATCH")
        .map(|write| write.body["url"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// An edit made while a write is in flight is accepted as a second write by
/// whatever leaves the editor.
fn assert_edit_during_a_write_is_kept(leave_by_back: bool, cx: &mut TestAppContext) {
    let gate = ResponseGate::new();
    let fixture = gated_fixture(Some(("PATCH", Arc::clone(&gate))));
    let (window, workspace, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| {
            set_url(page, "https://first.test", window, cx);
            page.request_save(window, cx);
        });
    })
    .expect("test window");
    gate.entered(cx);
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| {
            set_url(page, "https://typed-during.test", window, cx);
            if leave_by_back {
                page.back(window, cx);
            } else {
                page.prepare_close(window, cx);
            }
        });
    })
    .expect("test window");
    let settings = settings(&workspace, cx);
    let pending = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
    assert!(pending.is_some(), "closing waits for both accepted writes");
    gate.release();
    cx.foreground_executor().block_test(async move {
        let _ = pending.expect("close receipt").await;
    });
    settle(window, cx);
    assert_eq!(
        patched_urls(&fixture),
        ["https://first.test", "https://typed-during.test"],
        "the later edit is written after the first, in order"
    );
    assert_eq!(
        fixture.configs.lock().expect("configs")[1]["url"],
        "https://typed-during.test"
    );
}

#[gpui_kit::test]
fn an_edit_during_a_write_is_kept_on_close(cx: &mut TestAppContext) {
    assert_edit_during_a_write_is_kept(false, cx);
}

#[gpui_kit::test]
fn an_edit_during_a_write_is_kept_on_back(cx: &mut TestAppContext) {
    assert_edit_during_a_write_is_kept(true, cx);
}

#[gpui_kit::test]
fn an_edit_during_a_write_drains_on_app_quit(cx: &mut TestAppContext) {
    let gate = ResponseGate::new();
    let fixture = gated_fixture(Some(("PATCH", Arc::clone(&gate))));
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| {
            set_url(page, "https://first.test", window, cx);
            page.request_save(window, cx);
        });
    })
    .expect("test window");
    gate.entered(cx);
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| {
            set_url(page, "https://typed-during.test", window, cx);
            page.request_save(window, cx);
        });
    })
    .expect("test window");
    // Quit transfers the I/O of every accepted write.
    let quit = page.update(cx, |page, cx| page.take_quit_write(cx));
    gate.release();
    cx.executor().allow_parking();
    cx.foreground_executor().block_test(async move {
        quit.expect("accepted writes").await;
    });
    assert_eq!(
        patched_urls(&fixture),
        ["https://first.test", "https://typed-during.test"]
    );
}

#[gpui_kit::test]
fn no_save_is_accepted_behind_a_queued_delete(cx: &mut TestAppContext) {
    let gate = ResponseGate::new();
    let fixture = gated_fixture(Some(("DELETE", Arc::clone(&gate))));
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| page.delete("hosted".into(), window, cx));
    })
    .expect("test window");
    gate.entered(cx);
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| {
            // The editor is still open while the delete is in flight.
            set_url(page, "https://would-recreate.test", window, cx);
            page.request_save(window, cx);
            page.prepare_close(window, cx);
            // A second delete of the same server is refused too.
            page.delete("hosted".into(), window, cx);
        });
    })
    .expect("test window");
    gate.release();
    settle(window, cx);
    let sent = config_writes(&fixture);
    assert_eq!(sent.len(), 1, "one delete and no save behind it");
    assert!(sent[0].is("DELETE", "/api/config/mcp/hosted"));
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(page.deleting.is_empty());
        assert!(page.editor.is_none(), "the deleted server's editor closes");
        assert_eq!(page.servers().len(), 1);
        assert_eq!(page.notice.as_deref(), Some("\"hosted\" deleted"));
    });
}

#[gpui_kit::test]
fn a_draft_is_checked_then_created_and_becomes_the_saved_server(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.create_new(window, cx)
    });
    cx.read(|cx| {
        assert_eq!(
            page.read(cx).selected.as_deref(),
            Some("new-mcp-server"),
            "a draft starts under a free name"
        );
    });
    act(window, &page, cx, |page, window, cx| {
        let name = page.editor.as_ref().expect("editor").name.clone();
        name.update(cx, |input, cx| input.set_value("files", window, cx));
        set_command(page, "npx\n-y\nserver", window, cx);
        // A draft never autosaves.
        page.request_save(window, cx);
        page.create(window, cx);
    });
    assert!(
        config_writes(&fixture).is_empty(),
        "a taken name is refused locally"
    );
    cx.read(|cx| {
        assert_eq!(
            page.read(cx).notice.as_deref(),
            Some("A server with this name already exists")
        );
    });
    act(window, &page, cx, |page, window, cx| {
        let name = page.editor.as_ref().expect("editor").name.clone();
        name.update(cx, |input, cx| input.set_value("My Server", window, cx));
        page.add_pair(Pairs::Environment, "api key", "secret", window, cx);
        page.create(window, cx);
    });
    let sent = config_writes(&fixture);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].is("POST", "/api/config/mcp/my-server"));
    assert_eq!(
        sent[0].body,
        json!({
            "scope": "user",
            "type": "local",
            "command": ["npx", "-y", "server"],
            "url": "",
            "environment": {"API_KEY": "secret"},
            "headers": {},
            "oauth": {},
            "timeout": null,
            "codemode": null,
            "disabled": false,
            "protocol": null,
        })
    );
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.selected.as_deref(), Some("my-server"));
        assert_eq!(
            page.editor
                .as_ref()
                .and_then(|editor| editor.saved_name.as_deref()),
            Some("my-server")
        );
        assert_eq!(page.servers().len(), 3, "the list is read again");
        assert_eq!(
            page.notice.as_deref(),
            Some("MCP server \"my-server\" created."),
            "the server's own sentence is shown"
        );
    });
}

#[gpui_kit::test]
fn a_refused_create_shows_the_reason_and_keeps_the_draft(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.create_new(window, cx)
    });
    fixture.fail_writes.store(true, Ordering::Release);
    act(window, &page, cx, |page, window, cx| {
        set_command(page, "npx", window, cx);
        page.create(window, cx);
    });
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.notice.as_deref(), Some("fixture write failure"));
        let editor = page.editor.as_ref().expect("the draft stays open");
        assert!(editor.saved_name.is_none() && !editor.creating);
        assert_eq!(page.form(cx).expect("form").command, ["npx"]);
    });
}

#[gpui_kit::test]
fn a_runtime_switch_retires_the_editor_and_its_pending_save(cx: &mut TestAppContext) {
    let first = fixture();
    let second = fixture();
    let (window, workspace, page) = open(&first.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| {
            set_url(page, "https://old-server-edit.test", window, cx);
            page.request_save(window, cx);
        });
    })
    .expect("test window");
    workspace.update(cx, |workspace, _| {
        workspace.draft_projects.insert(
            RuntimeId::from(second.server.endpoint.as_str()),
            DIRECTORY.to_owned(),
        );
    });
    retarget(&workspace, &second.server, cx);
    settle(window, cx);
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(page.editor.is_none(), "the old server's editor is gone");
        assert_eq!(page.selected, None);
        assert!(!page.has_failed_writes());
        assert_eq!(page.servers().len(), 2, "the new server's list is read");
    });
    assert!(
        config_writes(&second).is_empty(),
        "the retired edit is never written to the new server"
    );
}

#[gpui_kit::test]
fn a_hidden_page_reads_nothing_when_the_owner_changes(cx: &mut TestAppContext) {
    let first = fixture();
    let second = fixture();
    let (window, workspace, page) = open(&first.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("files", window, cx);
        page.set_shown(false, cx);
    });
    let reads = cx.read(|cx| page.read(cx).read_revision);
    workspace.update(cx, |workspace, _| {
        workspace.draft_projects.insert(
            RuntimeId::from(second.server.endpoint.as_str()),
            DIRECTORY.to_owned(),
        );
    });
    retarget(&workspace, &second.server, cx);
    settle(window, cx);
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.read_revision, reads, "a hidden page starts no read");
        assert!(page.editor.is_none(), "leaving the page returns to the list");
        assert_eq!(page.list, Load::Loading);
    });
    assert!(second.server.requests().is_empty());
    act(window, &page, cx, |page, _, cx| page.set_shown(true, cx));
    cx.read(|cx| assert_eq!(page.read(cx).servers().len(), 2));
}

#[gpui_kit::test]
fn connect_and_disconnect_update_the_one_status_store(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, workspace, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("files", window, cx)
    });
    act(window, &page, cx, |page, window, cx| {
        page.toggle_connect(window, cx)
    });
    assert!(
        fixture
            .server
            .writes()
            .iter()
            .any(|request| request.is("POST", "/api/experimental/mcp/files/disconnect"))
    );
    assert_eq!(
        card_status(&workspace, "files", cx),
        McpConnection::Disabled,
        "the card's row follows the page's disconnect"
    );
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.notice.as_deref(), Some("Disconnected"));
        assert_eq!(page.busy, None);
    });
    // A test of a server that is off connects it, then switches it back off.
    act(window, &page, cx, |page, window, cx| {
        page.test_connection(window, cx)
    });
    let posts: Vec<String> = fixture
        .server
        .writes()
        .iter()
        .map(|request| request.target.split('?').next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        posts,
        [
            "/api/experimental/mcp/files/disconnect",
            "/api/experimental/mcp/files/connect",
            "/api/experimental/mcp/files/disconnect",
        ]
    );
    assert_eq!(card_status(&workspace, "files", cx), McpConnection::Disabled);
    cx.read(|cx| {
        assert_eq!(
            page.read(cx).notice.as_deref(),
            Some("Connection test succeeded")
        );
    });
    act(window, &page, cx, |page, window, cx| {
        page.toggle_connect(window, cx)
    });
    assert_eq!(card_status(&workspace, "files", cx), McpConnection::Connected);
    cx.read(|cx| assert_eq!(page.read(cx).notice.as_deref(), Some("Connected")));
}

#[gpui_kit::test]
fn a_change_made_from_the_card_shows_in_a_clean_editor(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("files", window, cx)
    });
    // The work status card's switch wrote `disabled` into the config file.
    fixture.configs.lock().expect("configs")[0]["disabled"] = json!(true);
    act(window, &page, cx, |page, _, cx| page.refresh(cx));
    settle(window, cx);
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(
            !page.form(cx).expect("form").enabled,
            "a clean form follows its entry on disk"
        );
        assert!(!page.editor.as_ref().expect("editor").saved.enabled);
    });
    assert!(config_writes(&fixture).is_empty());
    // An edit in progress is never replaced by a reread.
    act(window, &page, cx, |page, window, cx| {
        set_command(page, "npx\nediting", window, cx);
    });
    fixture.configs.lock().expect("configs")[0]["disabled"] = json!(false);
    act(window, &page, cx, |page, _, cx| page.refresh(cx));
    settle(window, cx);
    cx.read(|cx| {
        let form = page.read(cx).form(cx).expect("form");
        assert_eq!(form.command, ["npx", "editing"]);
        assert!(!form.enabled);
    });
}

#[gpui_kit::test]
fn a_policy_block_is_shown_and_takes_the_connect_action(cx: &mut TestAppContext) {
    let fixture = fixture();
    *fixture.policies.lock().expect("policies") = json!([
        {"type": "document", "info": {"policies": [
            {"action": "integration.use", "resource": "mcp:files", "effect": "deny"},
        ]}},
    ]);
    let (window, _, page) = open(&fixture.server, cx);
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.standing("files", cx), Some(Standing::Blocked));
        assert_ne!(page.standing("hosted", cx), Some(Standing::Blocked));
    });
    act(window, &page, cx, |page, window, cx| {
        page.select("files", window, cx)
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("mcp-status-card").is_some(),
            "a blocked server explains itself"
        );
    })
    .expect("test window");
}

#[gpui_kit::test]
fn enterprise_mode_leaves_mcp_entry_as_it_is(cx: &mut TestAppContext) {
    // The reference gates model providers, tunnels and the integrations
    // catalog in enterprise mode; MCP entry in Settings is unchanged, and
    // signing in to a remote server from the config stays allowed.
    let fixture = fixture();
    let (window, workspace, page) = open(&fixture.server, cx);
    workspace.update(cx, |workspace, cx| {
        workspace.enterprise.status = crate::enterprise::PolicyStatus {
            enabled: true,
            unavailable: false,
        };
        cx.notify();
    });
    settle(window, cx);
    assert!(cx.read(|cx| workspace.read(cx).enterprise.enterprise_mode()));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("mcp-create").is_some());
        assert!(window.try_find("mcp-open-hosted").is_some());
    })
    .expect("test window");
    act(window, &page, cx, |page, window, cx| {
        page.create_new(window, cx)
    });
    act(window, &page, cx, |page, window, cx| {
        set_command(page, "https://remote.test/mcp", window, cx);
        page.create(window, cx);
    });
    let sent = config_writes(&fixture);
    assert_eq!(sent.len(), 1, "a remote server is still created");
    assert_eq!(sent[0].body["type"], "remote");
}

#[gpui_kit::test]
fn a_sign_in_with_a_pasted_code_reconnects_the_server(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, workspace, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    cx.read(|cx| {
        let page = page.read(cx);
        let sign_in = &page.editor.as_ref().expect("editor").sign_in;
        assert_eq!(
            sign_in
                .integration
                .as_ref()
                .map(|integration| integration.id.as_str()),
            Some("mcp_hosted"),
            "a remote server with OAuth left on finds its integration"
        );
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("mcp-sign-in-start-0").is_some(),
            "a server waiting for sign-in offers it"
        );
    })
    .expect("test window");
    act(window, &page, cx, |page, window, cx| {
        page.start_sign_in(0, window, cx)
    });
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(matches!(
            page.editor.as_ref().expect("editor").sign_in.phase,
            SignInPhase::AwaitingCode {
                submitting: false,
                ..
            }
        ));
    });
    act(window, &page, cx, |page, window, cx| {
        let code = page.editor.as_ref().expect("editor").sign_in.code.clone();
        code.update(cx, |input, cx| input.set_value(" pasted ", window, cx));
        page.submit_code(window, cx);
    });
    let writes = fixture.server.writes();
    let complete = writes
        .iter()
        .find(|request| request.target.contains("/complete"))
        .expect("the code is handed over");
    assert_eq!(complete.body, json!({"code": "pasted"}));
    assert!(
        writes
            .iter()
            .any(|request| request.is("POST", "/api/experimental/mcp/hosted/connect")),
        "the stored credential is used by connecting again"
    );
    assert_eq!(card_status(&workspace, "hosted", cx), McpConnection::Connected);
    cx.read(|cx| {
        let page = page.read(cx);
        assert_eq!(page.notice.as_deref(), Some("Connected"));
        assert_eq!(
            page.editor.as_ref().expect("editor").sign_in.phase,
            SignInPhase::Idle
        );
    });
}

#[gpui_kit::test]
fn leaving_a_started_sign_in_drops_the_attempt_upstream(cx: &mut TestAppContext) {
    let fixture = fixture();
    let (window, _, page) = open(&fixture.server, cx);
    act(window, &page, cx, |page, window, cx| {
        page.select("hosted", window, cx)
    });
    act(window, &page, cx, |page, window, cx| {
        page.start_sign_in(0, window, cx)
    });
    act(window, &page, cx, |page, window, cx| page.back(window, cx));
    assert!(
        fixture.server.writes().iter().any(|request| request.is(
            "DELETE",
            "/api/integration/mcp_hosted/connect/oauth/attempt"
        )),
        "an abandoned attempt is cancelled"
    );
    cx.read(|cx| assert!(page.read(cx).editor.is_none()));
}
