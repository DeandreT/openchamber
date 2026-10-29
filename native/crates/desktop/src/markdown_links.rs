//! Untrusted links use the web classification and the native shell exclusions.
//!
//! Session routes stay in the current workspace. Files and message fragments
//! remain inside the client even while their native navigation is unavailable.
//! Only normalized HTTP(S), or an explicitly confirmed application URL, reach
//! the platform. This module does not fetch links or launch anything at render time.

use gpui_kit::{App, FollowMode, Global, ListOffset, PromptLevel, Window, px};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_transport::endpoint::Endpoint;

use crate::locale;
use crate::workspace::Workspace;

// packages/electron/external-url-policy.mjs and packages/ui/src/lib/url.ts.
const EXCLUDED: &[&str] = &[
    "mailto",
    "tel",
    "sms",
    "callto",
    "cid",
    "xmpp",
    "irc",
    "news",
    "nntp",
    "feed",
    "webcal",
    "javascript",
    "data",
    "vbscript",
    "blob",
    "filesystem",
    "about",
    "chrome",
    "chrome-extension",
    "devtools",
    "moz-extension",
    "ms-browser-extension",
    "file",
    "ws",
    "wss",
    "ftp",
    "ftps",
    "intent",
    "ms-msdt",
    "search-ms",
    "shell",
    "openchamber",
    "openchamber-ui",
    "capacitor",
];

#[derive(Debug, PartialEq, Eq)]
enum Link {
    External(String),
    Application(String),
    Session {
        session: String,
        message: Option<String>,
    },
    File,
    Fragment,
    Denied,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn session_link(session: &str, message: Option<String>) -> Link {
    if !identifier(session) || message.as_deref().is_some_and(|id| !identifier(id)) {
        return Link::Denied;
    }
    Link::Session {
        session: session.to_owned(),
        message,
    }
}

fn classify(raw: &str, endpoint: Option<&Endpoint>) -> Link {
    let value = raw.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Link::Denied;
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return Link::File;
    }
    if value.contains('\\') {
        return Link::Denied;
    }
    if value.starts_with('#') {
        return Link::Fragment;
    }
    let bare = value.strip_prefix('/').unwrap_or(value);
    if bare.starts_with("ses_") && identifier(bare) {
        return session_link(bare, None);
    }
    let Some((scheme, _)) = value.split_once(':') else {
        return Link::File;
    };
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
    {
        return Link::Denied;
    }
    // Endpoint exposes the transport's URL parser without another dependency.
    // The explicit scheme above ensures this join cannot resolve a relative URL.
    let base = Endpoint::parse("https://native.invalid/").expect("constant base URL");
    let Ok(url) = base.url("").join(value) else {
        return Link::Denied;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Link::Denied;
    }
    let message = || {
        url.query_pairs()
            .find(|(key, _)| key == "message")
            .map(|(_, id)| id.into_owned())
    };
    if url.scheme() == "openchamber" {
        let mut segments = Vec::new();
        if let Some(host) = url.host_str() {
            segments.push(host);
        }
        segments.extend(url.path().split('/').filter(|part| !part.is_empty()));
        return match segments.as_slice() {
            [route, session] if route.eq_ignore_ascii_case("session") => {
                session_link(session, message())
            }
            _ => Link::Denied,
        };
    }
    if matches!(url.scheme(), "http" | "https") {
        if url.host_str().is_none() {
            return Link::Denied;
        }
        if endpoint.is_some_and(|endpoint| endpoint.url("").origin() == url.origin())
            && let Some((_, session)) = url.query_pairs().find(|(key, _)| key == "session")
        {
            return session_link(&session, message());
        }
        return Link::External(url.into());
    }
    if url.scheme() == "file" {
        return Link::File;
    }
    if EXCLUDED.contains(&url.scheme()) || !(2..=32).contains(&url.scheme().len()) {
        return Link::Denied;
    }
    Link::Application(url.into())
}

#[derive(Default)]
struct ConfirmationSerial(u64);
impl Global for ConfirmationSerial {}

/// Preserves Kit's default link activation gestures when replacing its opener.
pub(crate) fn on_markdown_link_click(
    url: &gpui_kit::SharedString,
    event: &gpui_kit::ClickEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let activate = match event {
        gpui_kit::ClickEvent::Mouse(click) => {
            matches!(
                click.up.button,
                gpui_kit::MouseButton::Left | gpui_kit::MouseButton::Middle
            )
        }
        gpui_kit::ClickEvent::Keyboard(_) => true,
        gpui_kit::ClickEvent::Touch(click) => !click.long_press,
    };
    if activate {
        open_user_link(url, window, cx);
    }
}

fn workspace_owner(window: &Window, cx: &App) -> Option<gpui_kit::Entity<Workspace>> {
    window.root::<Workspace>().flatten().or_else(|| {
        window
            .root::<gpui_kit::base::Root>()
            .flatten()
            .and_then(|root| root.read(cx).view().clone().downcast::<Workspace>().ok())
    })
}

/// Opens an untrusted user link using native routing and explicit app confirmation.
pub(crate) fn open_user_link(url: &str, window: &mut Window, cx: &mut App) {
    let owner = workspace_owner(window, cx);
    let endpoint = owner.as_ref().and_then(|owner| {
        owner
            .read(cx)
            .connection
            .as_ref()
            .map(|connection| connection.config.endpoint.clone())
    });
    match classify(url, endpoint.as_ref()) {
        Link::External(url) => cx.open_url(&url),
        Link::Application(url) => {
            let Some(owner) = owner else { return };
            let runtime = owner.read(cx).runtime.clone();
            let selected = owner.read(cx).selected.clone();
            let owner = owner.downgrade();
            if cx.try_global::<ConfirmationSerial>().is_none() {
                cx.set_global(ConfirmationSerial::default());
            }
            let serial = &mut cx.global_mut::<ConfirmationSerial>().0;
            *serial += 1;
            let serial = *serial;
            let strings = locale::strings(cx);
            let title = strings.get("chat.appLink.confirm.title");
            let description = strings.get("chat.appLink.confirm.descriptionPlain");
            let cancel = strings.get("chat.appLink.confirm.cancel");
            let open = strings.get("chat.appLink.confirm.open");
            let detail = format!("{description}\n\n{url}");
            let answer = window.prompt(
                PromptLevel::Warning,
                &title,
                Some(&detail),
                &[cancel.as_ref(), open.as_ref()],
                cx,
            );
            window
                .spawn(cx, async move |cx| {
                    if answer.await != Ok(1) {
                        return;
                    }
                    let _ = cx.update(|window, cx| {
                        if cx.global::<ConfirmationSerial>().0 != serial {
                            return;
                        }
                        let Some(owner) = owner.upgrade() else {
                            return;
                        };
                        if workspace_owner(window, cx).as_ref() != Some(&owner) {
                            return;
                        }
                        let workspace = owner.read(cx);
                        if workspace.runtime == runtime && workspace.selected == selected {
                            // Classification is repeated at the actual platform boundary.
                            if let Link::Application(url) = classify(&url, None) {
                                cx.open_url(&url);
                            }
                        }
                    });
                })
                .detach();
        }
        Link::Session { session, message } => {
            let Some(owner) = owner else { return };
            owner.update(cx, |workspace, cx| {
                let session = SessionId::from(session.as_str());
                if workspace.session(&session).is_none() {
                    unavailable("contextPanel.tracker.unavailable", window, cx);
                    return;
                }
                workspace.select_session(session.clone(), window, cx);
                if let Some(message) = message {
                    let open = workspace.open_sessions.get(&session);
                    let row = open.and_then(|open| {
                        open.transcript.row_of(&MessageId::from(message.as_str()))
                    });
                    if let (Some(open), Some(row)) = (open, row) {
                        open.list.set_follow_mode(FollowMode::Normal);
                        open.list.scroll_to(ListOffset {
                            item_ix: row,
                            offset_in_item: px(0.0),
                        });
                        cx.notify();
                    } else {
                        unavailable("contextPanel.tracker.unavailable", window, cx);
                    }
                }
            });
        }
        Link::File => unavailable("mobile.files.error.readUnavailable", window, cx),
        Link::Fragment => unavailable("contextPanel.tracker.unavailable", window, cx),
        Link::Denied => {}
    }
}

fn unavailable(key: &str, window: &mut Window, cx: &mut App) {
    let strings = locale::strings(cx);
    let title = strings.get("common.unavailable");
    let detail = strings.get(key);
    let close = strings.get("dialog.common.actions.close");
    drop(window.prompt(
        PromptLevel::Info,
        &title,
        Some(&detail),
        &[close.as_ref()],
        cx,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, TestAppContext};

    #[gpui_kit::test]
    fn secondary_events_cannot_open_navigate_or_prompt_and_keyboard_still_activates(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{
            ClickEvent, KeyboardClickEvent, MouseButton, MouseClickEvent, MouseUpEvent,
            TouchClickEvent,
        };

        let (window, owner) = crate::workspace_tests::open(cx);
        let session = cx.read(|cx| {
            owner
                .read(cx)
                .sidebar_items
                .iter()
                .find_map(|item| match item {
                    crate::sidebar_item::SidebarItem::Session { session_id, .. } => {
                        Some(session_id.clone())
                    }
                    _ => None,
                })
                .expect("synthetic session")
        });
        let session_url: gpui_kit::SharedString = format!("openchamber://session/{session}").into();
        let app_url = "obsidian://vault/note".into();
        let web_url = "https://example.org/".into();
        for event in [
            ClickEvent::Mouse(MouseClickEvent {
                up: MouseUpEvent {
                    button: MouseButton::Right,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ClickEvent::Touch(TouchClickEvent {
                long_press: true,
                ..Default::default()
            }),
        ] {
            cx.update_window(window, |_, window, cx| {
                for url in [
                    &session_url,
                    &app_url,
                    &web_url,
                    &"file:///tmp/note.md".into(),
                    &"#note".into(),
                ] {
                    on_markdown_link_click(url, &event, window, cx);
                }
            })
            .expect("window exists");
            assert_eq!(cx.opened_url(), None);
            assert_eq!(cx.read(|cx| owner.read(cx).selected.clone()), None);
            assert!(!cx.has_pending_prompt());
        }
        let keyboard = ClickEvent::Keyboard(KeyboardClickEvent::default());
        cx.update_window(window, |_, window, cx| {
            on_markdown_link_click(&session_url, &keyboard, window, cx);
            on_markdown_link_click(&app_url, &keyboard, window, cx);
        })
        .expect("window exists");
        assert_eq!(cx.read(|cx| owner.read(cx).selected.clone()), Some(session));
        assert!(cx.has_pending_prompt());
        assert_eq!(cx.opened_url(), None);
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            on_markdown_link_click(&web_url, &keyboard, window, cx);
        })
        .expect("window exists");
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.org/"));
        cx.update_window(window, |_, window, cx| {
            on_markdown_link_click(
                &"https://example.org/tap".into(),
                &ClickEvent::Touch(TouchClickEvent::default()),
                window,
                cx,
            );
        })
        .expect("window exists");
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.org/tap"));
    }

    #[gpui_kit::test]
    fn platform_opens_application_only_after_confirmation(cx: &mut TestAppContext) {
        let (window, _) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            open_user_link("obsidian://vault/note", window, cx);
        })
        .expect("window exists");
        assert!(cx.has_pending_prompt());
        assert_eq!(cx.opened_url(), None);
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(cx.opened_url(), None);
        cx.update_window(window, |_, window, cx| {
            open_user_link("obsidian://vault/note", window, cx);
        })
        .expect("window exists");
        assert_eq!(cx.opened_url(), None);
        cx.simulate_prompt_answer("Open once");
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some("obsidian://vault/note"));
    }

    #[gpui_kit::test]
    fn stale_confirmation_cannot_open_for_another_runtime_or_session(cx: &mut TestAppContext) {
        let (window, owner) = crate::workspace_tests::open(cx);
        for runtime_switch in [false, true] {
            cx.update_window(window, |_, window, cx| {
                open_user_link("vscode://file/tmp/test", window, cx);
                owner.update(cx, |workspace, _| {
                    if runtime_switch {
                        workspace.runtime =
                            openchamber_native_core::ids::RuntimeId::from("changed-runtime");
                    } else {
                        workspace.selected = Some(SessionId::from("changed-session"));
                    }
                });
            })
            .expect("window exists");
            cx.simulate_prompt_answer("Open once");
            cx.run_until_parked();
            assert_eq!(cx.opened_url(), None);
        }
    }

    #[gpui_kit::test]
    fn superseded_confirmation_cannot_open_the_old_url(cx: &mut TestAppContext) {
        let (window, _) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            open_user_link("obsidian://vault/old", window, cx);
            open_user_link("obsidian://vault/new", window, cx);
        })
        .expect("window exists");
        cx.simulate_prompt_answer("Open once");
        cx.run_until_parked();
        assert_eq!(cx.opened_url(), None);
        cx.simulate_prompt_answer("Open once");
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some("obsidian://vault/new"));
    }

    #[gpui_kit::test]
    fn routes_sessions_and_keeps_files_fragments_and_dangerous_urls_inside(
        cx: &mut TestAppContext,
    ) {
        let (window, owner) = crate::workspace_tests::open(cx);
        let session = cx.read(|cx| {
            owner
                .read(cx)
                .sidebar_items
                .iter()
                .find_map(|item| {
                    if let crate::sidebar_item::SidebarItem::Session { session_id, .. } = item {
                        Some(session_id.clone())
                    } else {
                        None
                    }
                })
                .expect("synthetic session")
        });
        cx.update_window(window, |_, window, cx| {
            open_user_link(
                &format!("openchamber://session/{}", session.as_str()),
                window,
                cx,
            );
        })
        .expect("window exists");
        assert_eq!(
            cx.read(|cx| owner.read(cx).selected.clone()),
            Some(session.clone())
        );
        let message = cx.read(|cx| {
            owner.read(cx).open_sessions[&session]
                .transcript
                .message(1)
                .expect("synthetic second message")
                .info
                .id
                .clone()
        });
        assert!(
            identifier(message.as_str()),
            "synthetic message must be URL-safe: {message}"
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            open_user_link(
                &format!(
                    "openchamber://session/{}?message={}",
                    session.as_str(),
                    message.as_str()
                ),
                window,
                cx,
            );
            assert_eq!(
                owner.read(cx).open_sessions[&session]
                    .list
                    .logical_scroll_top()
                    .item_ix,
                1
            );
        })
        .expect("window exists");
        assert!(!cx.read(|cx| {
            owner.read(cx).open_sessions[&session]
                .list
                .is_following_tail()
        }));
        for url in [
            "file:///etc/passwd",
            "../notes.md",
            "#footnote",
            "openchamber://session/missing",
        ] {
            cx.update_window(window, |_, window, cx| open_user_link(url, window, cx))
                .expect("window exists");
            assert!(cx.has_pending_prompt(), "{url} reports unavailable");
            cx.simulate_prompt_answer("Close");
            assert_eq!(cx.opened_url(), None);
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,test",
            "shell:AppsFolder",
            "mailto:test@example.com",
        ] {
            cx.update_window(window, |_, window, cx| open_user_link(url, window, cx))
                .expect("window exists");
            assert!(!cx.has_pending_prompt());
            assert_eq!(cx.opened_url(), None);
        }
        cx.update_window(window, |_, window, cx| {
            open_user_link("https://example.org", window, cx)
        })
        .expect("window exists");
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.org/"));
    }

    #[gpui_kit::test]
    fn unknown_window_owner_cannot_launch_internal_or_application_links(cx: &mut TestAppContext) {
        struct Other;
        impl gpui_kit::Render for Other {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut gpui_kit::Context<Self>,
            ) -> impl gpui_kit::IntoElement {
                gpui_kit::div()
            }
        }
        let (window, _owner) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            open_user_link("obsidian://vault/pending", window, cx);
            window.replace_root(cx, |_, _| Other);
            for url in [
                "obsidian://vault/note",
                "openchamber://session/ses_123",
                "ses_123",
            ] {
                open_user_link(url, window, cx);
            }
        })
        .expect("window exists");
        cx.simulate_prompt_answer("Open once");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(cx.opened_url(), None);
    }

    #[test]
    fn native_shell_exclusions_never_become_application_links() {
        for scheme in EXCLUDED {
            let link = classify(&format!("{scheme}://example/test"), None);
            assert!(
                !matches!(link, Link::External(_) | Link::Application(_)),
                "{scheme}"
            );
        }
        for value in [
            "java\nscript:alert(1)",
            "https://user:password@example.com",
            "https://",
            "x:test",
            "data:text/html,test",
        ] {
            assert_eq!(classify(value, None), Link::Denied, "{value}");
        }
    }

    #[test]
    fn classifies_external_and_internal_routes() {
        let endpoint = Endpoint::parse("https://example.com/oc").expect("valid endpoint");
        assert_eq!(
            classify("https://example.org", Some(&endpoint)),
            Link::External("https://example.org/".into())
        );
        assert_eq!(
            classify("obsidian://vault/note", None),
            Link::Application("obsidian://vault/note".into())
        );
        for value in [
            "ses_123",
            "/ses_123",
            "openchamber://session/ses_123",
            "openchamber:///session/ses_123",
        ] {
            assert_eq!(classify(value, None), session_link("ses_123", None));
        }
        assert_eq!(
            classify(
                "https://example.com/oc/?session=ses_123&message=msg_1",
                Some(&endpoint)
            ),
            session_link("ses_123", Some("msg_1".into()))
        );
        assert!(matches!(
            classify("https://other.example/?session=ses_123", Some(&endpoint)),
            Link::External(_)
        ));
        assert_eq!(
            classify("openchamber://session/ses_123?message=bad%20id", None),
            Link::Denied
        );
        assert_eq!(classify("file:///tmp/test.md", None), Link::File);
        assert_eq!(classify("../test.md", None), Link::File);
        assert_eq!(classify("C:/notes/test.md", None), Link::File);
        assert_eq!(classify("C:\\notes\\test.md", None), Link::File);
        assert_eq!(classify("#note", None), Link::Fragment);
    }
}
