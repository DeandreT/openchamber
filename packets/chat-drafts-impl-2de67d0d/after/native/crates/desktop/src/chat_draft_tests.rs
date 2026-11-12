//! The Chat draft's destination, key and text against a workspace that has a
//! server connection but sends nothing: no request leaves these tests.

use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};
use openchamber_native_core::chat_draft::{self, CHAT_DRAFT_ID, CHAT_PROJECT_ID, RootsStatus};
use openchamber_native_core::chat_roots::ChatRoots;
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_transport::endpoint::Endpoint;

use super::NewSession;
use crate::connection::{Connection, ServerConfig};
use crate::workspace::Workspace;

/// A workspace with the synthetic projects and Chats root and a connection
/// that was never started, so a Chat can be targeted.
fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            let endpoint = Endpoint::parse("http://127.0.0.1:9").expect("unused test endpoint");
            workspace.connection =
                Some(Connection::new(ServerConfig::for_endpoint(endpoint)).expect("network"));
            // The synthetic roots stand for a root the server already named.
            workspace.session_admission.roots_ready = true;
        });
    });
    (window, workspace)
}

fn roots(workspace: &Workspace) -> ChatRoots {
    workspace
        .topology
        .chat_roots
        .clone()
        .expect("the synthetic workspace has Chats roots")
}

fn type_text(
    workspace: &mut Workspace,
    text: &str,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::Context<Workspace>,
) {
    workspace.composer.update(cx, |composer, cx| {
        composer.set_value(text.to_owned(), window, cx)
    });
    workspace.save_composer_draft(cx);
}

fn composer_text(workspace: &Workspace, cx: &gpui_kit::App) -> String {
    workspace.composer.read(cx).value().to_string()
}

#[gpui_kit::test]
fn a_plain_new_session_opens_a_chat_in_its_own_folder_under_the_root(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_new_session(NewSession::Plain, window, cx);
            assert!(workspace.chat_draft_open());
            assert!(!workspace.draft_projects.contains_key(&workspace.runtime));
            let roots = roots(workspace);
            let key = workspace.composer_key.clone().expect("the draft has a key");
            assert_eq!(Some(&key), workspace.chat_draft_key().as_ref());
            assert_eq!(key.target(), &DraftTarget::NewSession(CHAT_DRAFT_ID.into()));
            // The session is created in the key's directory: a folder of its
            // own, never the shared root and never a project.
            assert!(chat_draft::is_session_folder(&roots, key.directory()));
            assert!(workspace.is_chat_creation(&key));
            assert!(
                workspace
                    .draft_destination_project(key.directory())
                    .is_none()
            );
            // Catalogs, panels and the terminal work in the root meanwhile.
            assert_eq!(
                workspace.config_directory().as_deref(),
                Some(roots.configured())
            );
            assert_eq!(workspace.chat_roots_status(), RootsStatus::Ready);
        });
    })
    .expect("test window");
}

#[gpui_kit::test]
fn the_shortcut_keeps_a_project_session_in_its_project_and_the_chats_plus_opens_a_chat(
    cx: &mut TestAppContext,
) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let project = workspace.topology.projects[0].path.clone();
            workspace.open_new_session(NewSession::Plain, window, cx);
            workspace.move_draft_to(&project, window, cx);
            assert!(!workspace.chat_draft_open());
            assert_eq!(
                workspace.draft_projects.get(&workspace.runtime),
                Some(&project)
            );
            // The project header's "+" and the picker both leave a project
            // target, which a plain new session reopens.
            workspace.open_new_session(NewSession::Plain, window, cx);
            assert!(!workspace.chat_draft_open());
            assert_eq!(
                workspace.draft_projects.get(&workspace.runtime),
                Some(&project)
            );
            // The Chats header's "+" asks for a Chat whatever came before.
            workspace.open_new_session(NewSession::Chat, window, cx);
            assert!(workspace.chat_draft_open());
            assert!(!workspace.draft_projects.contains_key(&workspace.runtime));
            // With no session selected the shortcut forwards no directory
            // and reopens the side last worked on, which is Chat now.
            workspace.open_new_session(NewSession::FromSelected, window, cx);
            assert!(workspace.chat_draft_open());
        });
    })
    .expect("test window");
}

#[gpui_kit::test]
fn switching_the_draft_target_keeps_the_text(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let project = workspace.topology.projects[0].path.clone();
            workspace.open_new_session(NewSession::Chat, window, cx);
            let chat = workspace.composer_key.clone().expect("chat key");
            type_text(workspace, "typed in the chat", window, cx);

            workspace.move_draft_to(&project, window, cx);
            let in_project = workspace.composer_key.clone().expect("project key");
            assert_eq!(in_project.directory(), project);
            assert_eq!(composer_text(workspace, cx), "typed in the chat");
            assert_eq!(
                workspace
                    .drafts
                    .get(&in_project)
                    .map(|draft| draft.text.as_str()),
                Some("typed in the chat")
            );
            assert!(
                workspace.drafts.get(&chat).is_none(),
                "the text moved; it was not copied"
            );

            workspace.move_draft_to(CHAT_PROJECT_ID, window, cx);
            assert!(workspace.chat_draft_open());
            let back = workspace.composer_key.clone().expect("chat key");
            assert_eq!(composer_text(workspace, cx), "typed in the chat");
            assert_eq!(
                workspace.drafts.get(&back).map(|draft| draft.text.as_str()),
                Some("typed in the chat")
            );
            assert!(workspace.drafts.get(&in_project).is_none());
        });
    })
    .expect("test window");
}

#[gpui_kit::test]
fn a_new_draft_opened_elsewhere_leaves_the_chat_text_in_place(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let project = workspace.topology.projects[0].path.clone();
            workspace.open_new_session(NewSession::Chat, window, cx);
            let chat = workspace.composer_key.clone().expect("chat key");
            type_text(workspace, "chat text", window, cx);
            // A project's own new-session entry opens another draft.
            workspace.show_new_session(window, cx);
            workspace.choose_draft_project(&project, window, cx);
            assert_eq!(composer_text(workspace, cx), "");
            assert_eq!(
                workspace.drafts.get(&chat).map(|draft| draft.text.as_str()),
                Some("chat text")
            );
            // Coming back to Chat finds the folder its text is stored under.
            workspace.open_new_session(NewSession::Chat, window, cx);
            assert_eq!(workspace.composer_key.as_ref(), Some(&chat));
            assert_eq!(composer_text(workspace, cx), "chat text");
        });
    })
    .expect("test window");
}

#[gpui_kit::test]
fn a_failed_root_read_keeps_the_chat_draft_and_creates_nothing_elsewhere(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let named = roots(workspace);
            // The read failed: no roots, and no answer either.
            workspace.topology.chat_roots = None;
            workspace.session_admission.roots_ready = false;
            workspace.chat_roots_read(false, window, cx);
            assert_eq!(workspace.chat_roots_status(), RootsStatus::Failed);

            workspace.open_new_session(NewSession::Chat, window, cx);
            assert!(workspace.chat_draft_open(), "the draft stays a Chat");
            assert_eq!(workspace.chat_draft_key(), None);
            assert_eq!(
                workspace.config_directory(),
                None,
                "no project's catalogs stand in for the unknown root"
            );
            type_text(workspace, "kept through the failure", window, cx);
            let holding = workspace.composer_key.clone().expect("the text has a key");
            assert!(!workspace.is_chat_creation(&holding));

            // Sending stops at the failure and leaves everything in place.
            assert!(workspace.chat_send_blocked(window, cx));
            assert!(!workspace.submit_new_session_with(None, window, cx));
            assert!(!workspace.can_create_connected());
            assert_eq!(workspace.creations.iter().count(), 0);
            assert!(workspace.chat_draft_open());
            assert_eq!(composer_text(workspace, cx), "kept through the failure");
            assert_eq!(
                workspace
                    .drafts
                    .get(&holding)
                    .map(|draft| draft.text.as_str()),
                Some("kept through the failure")
            );

            // A server that answers without a root is a different state.
            workspace.session_admission.roots_ready = true;
            assert_eq!(workspace.chat_roots_status(), RootsStatus::Missing);
            assert!(workspace.chat_send_blocked(window, cx));
            workspace.session_admission.roots_ready = false;

            // The root arrives: the draft gets its folder and its text.
            workspace.topology.chat_roots = Some(named.clone());
            workspace.session_admission.roots_ready = true;
            workspace.chat_roots_read(true, window, cx);
            let key = workspace.chat_draft_key().expect("the folder is named");
            assert!(chat_draft::is_session_folder(&named, key.directory()));
            assert_eq!(workspace.composer_key.as_ref(), Some(&key));
            assert_eq!(composer_text(workspace, cx), "kept through the failure");
            assert!(workspace.drafts.get(&holding).is_none());
            assert!(!workspace.chat_send_blocked(window, cx));
        });
    })
    .expect("test window");
}

#[gpui_kit::test]
fn an_unsent_chat_draft_comes_back_under_its_folder_after_a_restart(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            // What the journal restores: a Chat draft saved under its folder.
            let roots = roots(workspace);
            let folder = chat_draft::session_directory(
                &roots,
                chat_draft::ChatDate {
                    year: 2026,
                    month: 11,
                    day: 10,
                },
                "restored",
            )
            .expect("the id is usable");
            let stored = DraftKey::new(
                workspace.runtime.clone(),
                &folder,
                DraftTarget::NewSession(CHAT_DRAFT_ID.into()),
            )
            .expect("the folder is usable");
            workspace
                .drafts
                .save(stored.clone(), Draft::new("unsent before the restart"));

            // The first draft of the runtime defaults to Chat and takes the
            // stored folder, not a new one.
            workspace.open_default_draft(window, cx);
            assert!(workspace.chat_draft_open());
            assert_eq!(workspace.composer_key.as_ref(), Some(&stored));
            assert_eq!(composer_text(workspace, cx), "unsent before the restart");

            // A later settings read does not move a draft the user retargets.
            let project = workspace.topology.projects[0].path.clone();
            workspace.show_new_session(window, cx);
            workspace.choose_draft_project(&project, window, cx);
            workspace.open_default_draft(window, cx);
            assert!(!workspace.chat_draft_open());
        });
    })
    .expect("test window");
}

#[gpui_kit::test]
fn a_chat_folder_outside_the_current_roots_is_replaced(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_new_session(NewSession::Chat, window, cx);
            let first = workspace.chat_draft_key().expect("chat key");
            type_text(workspace, "moves with the root", window, cx);
            // The server now names another root, as a relocated data
            // directory does; the old folder is no managed Chat folder.
            let moved = ChatRoots::from_home_info(Some("/elsewhere"), None, None, None)
                .expect("home is usable");
            workspace.topology.chat_roots = Some(moved.clone());
            workspace.chat_roots_read(true, window, cx);
            let second = workspace.chat_draft_key().expect("chat key");
            assert_ne!(first, second);
            assert!(chat_draft::is_session_folder(&moved, second.directory()));
            assert_eq!(composer_text(workspace, cx), "moves with the root");
        });
    })
    .expect("test window");
}
