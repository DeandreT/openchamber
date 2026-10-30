//! Session row menu, rename and management outcomes through real hit
//! testing, keyboard dispatch and the workspace's completion handlers.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext, ElementId, Entity, Focusable, SharedString, TestAppContext,
};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::session::MetadataFacts;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::session_actions::{Archived, Deleted, Restored};

use crate::send_attempt::SessionKey;
use crate::sidebar_item::SidebarItem;
use crate::workspace::Workspace;
use crate::workspace_tests::open;

fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

fn rows(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<(SharedString, SessionId)> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .sidebar_items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Session {
                    key, session_id, ..
                } => Some((key.clone(), session_id.clone())),
                _ => None,
            })
            .collect()
    })
}

fn listed(workspace: &Entity<Workspace>, session: &SessionId, cx: &mut TestAppContext) -> bool {
    rows(workspace, cx).iter().any(|(_, id)| id == session)
}

fn update(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>),
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| f(workspace, window, cx));
        window.render_frame(cx);
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_more_button_opens_the_menu_in_reference_order(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, session) = rows(&workspace, cx)[0].clone();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(name(format!("{key}:target")), cx);
        window.render_frame(cx);
        window.click(name(format!("{key}:menu")), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("session-menu")).is_some());
    })
    .expect("window is open");
    cx.read(|cx| {
        let state = workspace.read(cx);
        let menu = state.session_menu.as_ref().expect("the menu is open");
        assert_eq!(menu.session_id, session);
        assert_eq!(
            menu.highlighted, None,
            "a pointer opens without a highlight"
        );
        assert_eq!(menu.position, None, "the More button anchors the menu");
        let actions: Vec<_> = menu.entries.iter().map(|entry| entry.action()).collect();
        use openchamber_native_core::session_actions::SessionAction::*;
        assert_eq!(actions[..2], [Some(Rename), Some(Pin)]);
        assert_eq!(actions.last(), Some(&Some(Delete)));
        assert!(
            actions.contains(&None),
            "a separator sets archive and delete apart"
        );
        assert!(
            menu.entries.iter().skip(1).all(|entry| !entry.is_enabled()),
            "synthetic data has no server to change"
        );
    });
    cx.update_window(window, |_, window, cx| {
        // A second click on the trigger closes it again.
        window.hover(name(format!("{key}:target")), cx);
        window.render_frame(cx);
        window.click(name(format!("{key}:menu")), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("session-menu")).is_none());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn a_secondary_click_opens_the_menu_at_the_pointer(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, _) = rows(&workspace, cx)[1].clone();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(name(format!("{key}:target")), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("session-menu-item:rename")).is_some());
    })
    .expect("window is open");
    cx.read(|cx| {
        let menu = workspace.read(cx).session_menu.as_ref().expect("open");
        assert_eq!(menu.row_key, key);
        assert!(menu.position.is_some());
    });
    cx.update_window(window, |_, window, cx| {
        let focused = workspace
            .read(cx)
            .session_menu
            .as_ref()
            .map(|menu| menu.focus.is_focused(window));
        assert_eq!(focused, Some(true), "the open menu takes focus");
    })
    .expect("window is open");
    assert_eq!(
        cx.read(|cx| workspace.read(cx).selected.clone()),
        None,
        "a secondary click does not open the session"
    );
}

#[gpui_kit::test]
fn the_keyboard_opens_navigates_and_returns_focus(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, _) = rows(&workspace, cx)[0].clone();
    let trigger = name(format!("{key}:menu"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for _ in 0..80 {
            window.press("tab", cx);
            window.render_frame(cx);
            if window.find(trigger.clone()).focused() == Some(true) {
                break;
            }
        }
        assert_eq!(window.find(trigger.clone()).focused(), Some(true));
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    cx.read(|cx| {
        let menu = workspace
            .read(cx)
            .session_menu
            .as_ref()
            .expect("Enter opens it");
        assert_eq!(menu.highlighted, Some(0), "the keyboard highlights Rename");
    });
    cx.update_window(window, |_, window, cx| {
        // Only Rename is enabled offline; arrows and Ctrl+N/P wrap to it.
        for key in ["down", "ctrl-n", "up", "ctrl-p", "end", "home"] {
            window.press(key, cx);
        }
        window.render_frame(cx);
    })
    .expect("window is open");
    assert_eq!(
        cx.read(|cx| workspace
            .read(cx)
            .session_menu
            .as_ref()
            .and_then(|menu| menu.highlighted)),
        Some(0)
    );
    cx.update_window(window, |_, window, cx| {
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find(name("session-menu")).is_none());
        assert_eq!(
            window.find(trigger.clone()).focused(),
            Some(true),
            "Escape returns focus to the trigger"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn rename_from_the_menu_selects_the_title_and_escape_cancels(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, session) = rows(&workspace, cx)[0].clone();
    let title = cx.read(|cx| {
        workspace
            .read(cx)
            .sessions
            .get(&session)
            .expect("listed")
            .title
            .clone()
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(name(format!("{key}:target")), cx);
        window.render_frame(cx);
        window.click(name(format!("{key}:menu")), cx);
        window.render_frame(cx);
        window.click(name("session-menu-item:rename"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("session-menu")).is_none());
        assert!(window.try_find(name(format!("{key}:rename"))).is_some());
        let focused = workspace
            .read(cx)
            .session_rename
            .as_ref()
            .map(|rename| rename.input.read(cx).focus_handle(cx).is_focused(window));
        assert_eq!(focused, Some(true), "the field takes focus");
    })
    .expect("window is open");
    cx.read(|cx| {
        let rename = workspace.read(cx).session_rename.as_ref().expect("editing");
        let input = rename.input.read(cx);
        assert_eq!(input.value().as_ref(), title);
        assert_eq!(
            input.selected_text().to_string(),
            title,
            "the whole title is selected"
        );
    });
    cx.update_window(window, |_, window, cx| {
        window.input("Replaced", cx);
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find(name(format!("{key}:rename"))).is_none());
    })
    .expect("window is open");
    assert_eq!(
        cx.read(|cx| workspace
            .read(cx)
            .sessions
            .get(&session)
            .expect("listed")
            .title
            .clone()),
        title,
        "a cancelled rename changes nothing"
    );
}

#[gpui_kit::test]
fn double_click_renames_and_enter_or_an_empty_title_closes(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, _) = rows(&workspace, cx)[2].clone();
    for text in ["   ", "A new title"] {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.double_click(name(format!("{key}:target")), cx);
        })
        .expect("window is open");
        // The rename opens once the click's own selection has settled.
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(name(format!("{key}:rename"))).is_some());
            let focused = workspace
                .read(cx)
                .session_rename
                .as_ref()
                .map(|rename| rename.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(focused, Some(true), "{text:?}: the field takes focus");
            window.input(text, cx);
            window.press("enter", cx);
        })
        .expect("window is open");
        // Enter arrives through the field's event, after the update.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(name(format!("{key}:rename"))).is_none());
        })
        .expect("window is open");
        assert!(cx.read(|cx| workspace.read(cx).session_rename.is_none()));
    }
    // Synthetic data has no server: nothing was sent.
    assert!(cx.read(|cx| workspace.read(cx).session_management.renaming.is_empty()));
}

fn notice(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .session_management
            .notice
            .as_ref()
            .map(ToString::to_string)
    })
}

fn archive_one(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    session: &SessionId,
    reopen: Option<SessionId>,
    cx: &mut TestAppContext,
) {
    let runtime = runtime(workspace, cx);
    let session = session.clone();
    update(window, workspace, cx, |state, window, cx| {
        state.finish_archive(
            runtime,
            vec![session.clone()],
            reopen,
            Some(vec![(
                vec![session.clone()],
                Ok(Archived {
                    archived: vec![(session.clone(), 7)],
                    failed: Vec::new(),
                }),
            )]),
            window,
            cx,
        )
    });
}

#[gpui_kit::test]
fn undo_of_a_closed_session_reports_only_failures(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[1].clone();
    let runtime = runtime(&workspace, cx);
    // Archived while another view was open: Undo has nothing to reopen.
    archive_one(window, &workspace, &session, None, cx);
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_restore(
            runtime.clone(),
            vec![session.clone()],
            vec![session.clone()],
            None,
            true,
            Some(vec![(
                session.clone(),
                Ok(Restored {
                    restored: vec![session.clone()],
                    failed: Vec::new(),
                }),
            )]),
            window,
            cx,
        )
    });
    assert!(listed(&workspace, &session, cx));
    assert_ne!(
        notice(&workspace, cx).as_deref(),
        Some("Session restored"),
        "a successful undo is silent, as undoArchive is"
    );
}

#[gpui_kit::test]
fn undo_that_cannot_start_says_so_and_keeps_its_toast(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[1].clone();
    archive_one(window, &workspace, &session, None, cx);
    let mut started = true;
    update(window, &workspace, cx, |state, window, cx| {
        // Synthetic data has no server, so the restore cannot start.
        let runtime = state.runtime.clone();
        started = state.undo_archive(&runtime, vec![session.clone()], None, window, cx);
    });
    assert!(!started, "the toast stays so Undo can be tried again");
    assert_eq!(
        notice(&workspace, cx).as_deref(),
        Some("Failed to restore session")
    );
}

#[gpui_kit::test]
fn archiving_the_open_session_keeps_the_archive_page_open(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[0].clone();
    update(window, &workspace, cx, |state, window, cx| {
        state.select_session(session.clone(), window, cx);
        state.archive_open = true;
    });
    archive_one(window, &workspace, &session, Some(session.clone()), cx);
    cx.read(|cx| {
        let state = workspace.read(cx);
        assert_eq!(state.selected, None, "the archived session closes");
        assert!(state.archive_open, "the archive page stays where it was");
    });
}

#[gpui_kit::test]
fn a_session_being_renamed_does_not_open_another_editor(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, session) = rows(&workspace, cx)[2].clone();
    update(window, &workspace, cx, |state, _, _| {
        let key = state.mutation_key(&session);
        state.session_management.renaming.insert(key);
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.double_click(name(format!("{key}:target")), cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    assert!(
        cx.read(|cx| workspace.read(cx).session_rename.is_none()),
        "a second title would be dropped while the first is in flight"
    );
}

#[gpui_kit::test]
fn never_ask_without_a_server_keeps_the_stored_choice(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    update(window, &workspace, cx, |state, window, cx| {
        state.send_catalog.settings = Some(Default::default());
        // No server answers in synthetic data, so nothing can be stored.
        state.toggle_deletion_dialog(window, cx);
    });
    cx.read(|cx| {
        let state = workspace.read(cx);
        assert_eq!(
            state
                .send_catalog
                .settings
                .as_ref()
                .and_then(|settings| settings.show_deletion_dialog),
            None
        );
        assert!(state.shows_deletion_dialog(), "the dialog still asks");
    });
}

#[test]
fn quick_never_ask_clicks_follow_the_newest_choice() {
    use crate::session_management::DeletionDialogPreference;
    let mut preference = DeletionDialogPreference::default();
    // Stored: ask. The first click saves "never ask" at once.
    assert_eq!(preference.toggle(true), Some(false));
    assert!(!preference.shows(true));
    // A second click before the answer flips back and waits its turn.
    assert_eq!(preference.toggle(true), None, "one save at a time");
    assert!(
        preference.shows(true),
        "the checkbox follows the newest click"
    );
    // The first save landed; the newest choice still differs, so it is sent.
    assert_eq!(preference.finish(Some(false)), Some(true));
    assert_eq!(preference.finish(Some(true)), None);
    assert!(preference.shows(true));
    // A failed save shows the stored choice again and sends nothing more.
    assert_eq!(preference.toggle(true), Some(false));
    assert_eq!(preference.finish(None), None);
    assert!(preference.shows(true));
}

#[gpui_kit::test]
fn undo_after_a_runtime_switch_reports_and_dismisses(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[1].clone();
    archive_one(window, &workspace, &session, None, cx);
    let mut dismiss = false;
    update(window, &workspace, cx, |state, window, cx| {
        dismiss = state.undo_archive(
            &RuntimeId::from("another-server"),
            vec![session.clone()],
            None,
            window,
            cx,
        );
    });
    assert!(dismiss, "the toast belongs to a server no longer shown");
    assert_eq!(
        notice(&workspace, cx).as_deref(),
        Some("Failed to restore session")
    );
}

#[gpui_kit::test]
fn a_vanished_selection_under_the_archive_page_moves_no_focus(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[0].clone();
    let mut revision = 0;
    update(window, &workspace, cx, |state, window, cx| {
        state.select_session(session.clone(), window, cx);
        state.archive_open = true;
        window.focus(&state.focus_handle, cx);
        revision = state.navigation_revision;
    });
    archive_one(window, &workspace, &session, Some(session.clone()), cx);
    cx.update_window(window, |_, window, cx| {
        let state = workspace.read(cx);
        assert_eq!(state.selected, None);
        assert_eq!(
            state.navigation_revision, revision,
            "no navigation happened"
        );
        assert!(
            !state.composer.read(cx).focus_handle(cx).is_focused(window),
            "the hidden composer does not take focus"
        );
    })
    .expect("window is open");
}

fn key(workspace: &Entity<Workspace>, session: &SessionId, cx: &mut TestAppContext) -> SessionKey {
    cx.read(|cx| workspace.read(cx).mutation_key(session))
}

fn runtime(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> RuntimeId {
    cx.read(|cx| workspace.read(cx).runtime.clone())
}

#[gpui_kit::test]
fn a_confirmed_archive_moves_the_session_and_leaves_its_view(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[0].clone();
    let runtime = runtime(&workspace, cx);
    update(window, &workspace, cx, |state, window, cx| {
        state.select_session(session.clone(), window, cx)
    });
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_archive(
            runtime.clone(),
            vec![session.clone()],
            Some(session.clone()),
            Some(vec![(
                vec![session.clone()],
                Ok(Archived {
                    archived: vec![(session.clone(), 7)],
                    failed: Vec::new(),
                }),
            )]),
            window,
            cx,
        )
    });
    cx.read(|cx| {
        let state = workspace.read(cx);
        assert_eq!(
            state.selected, None,
            "the open session closes, as the web does"
        );
        let record = state.sessions.get(&session).expect("held as archived");
        assert_eq!(record.time.archived, Some(7));
    });
    assert!(!listed(&workspace, &session, cx));
    // The archive page lists it with Restore.
    update(window, &workspace, cx, |state, window, cx| {
        state.toggle_archive_page(window, cx)
    });
    cx.update_window(window, |_, window, _| {
        assert!(
            window
                .try_find(name(format!("archive-restore:{session}")))
                .is_some()
        );
    })
    .expect("window is open");
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_restore(
            runtime.clone(),
            vec![session.clone()],
            vec![session.clone()],
            None,
            false,
            Some(vec![(
                session.clone(),
                Ok(Restored {
                    restored: vec![session.clone()],
                    failed: Vec::new(),
                }),
            )]),
            window,
            cx,
        )
    });
    assert!(
        listed(&workspace, &session, cx),
        "a restored session returns to its list"
    );
}

#[gpui_kit::test]
fn a_failed_archive_keeps_the_session_and_its_selection(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[0].clone();
    let runtime = runtime(&workspace, cx);
    update(window, &workspace, cx, |state, window, cx| {
        state.select_session(session.clone(), window, cx);
        state
            .session_management
            .removing
            .insert(state.mutation_key(&session));
    });
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_archive(
            runtime.clone(),
            vec![session.clone()],
            Some(session.clone()),
            Some(vec![(
                vec![session.clone()],
                Err(TransportError::Http { status: 500 }),
            )]),
            window,
            cx,
        )
    });
    cx.read(|cx| {
        let state = workspace.read(cx);
        assert_eq!(state.selected.as_ref(), Some(&session));
        assert!(
            state.session_management.removing.is_empty(),
            "the row is actionable again"
        );
    });
    assert!(listed(&workspace, &session, cx));
}

#[gpui_kit::test]
fn one_failed_delete_leaves_the_others_and_a_stale_runtime_changes_nothing(
    cx: &mut TestAppContext,
) {
    let (window, workspace) = open(cx);
    let all = rows(&workspace, cx);
    let (first, second) = (all[0].1.clone(), all[1].1.clone());
    let runtime = runtime(&workspace, cx);
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_delete(
            RuntimeId::from("another-server"),
            vec![first.clone()],
            Some(vec![(first.clone(), Ok(Deleted::Removed))]),
            window,
            cx,
        )
    });
    assert!(
        listed(&workspace, &first, cx),
        "a stale answer never edits this runtime"
    );
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_delete(
            runtime.clone(),
            vec![first.clone(), second.clone()],
            Some(vec![
                (first.clone(), Ok(Deleted::AlreadyGone)),
                (second.clone(), Err(TransportError::TimedOut)),
            ]),
            window,
            cx,
        )
    });
    assert!(
        !listed(&workspace, &first, cx),
        "already gone counts as deleted"
    );
    assert!(listed(&workspace, &second, cx));
    assert!(cx.read(|cx| workspace.read(cx).sessions.get(&first).is_none()));
}

#[gpui_kit::test]
fn a_pin_shows_at_once_and_a_failure_flips_it_back(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (_, session) = rows(&workspace, cx)[3].clone();
    let key = key(&workspace, &session, cx);
    let pinned_row = |workspace: &Entity<Workspace>, cx: &mut TestAppContext| {
        cx.read(|cx| {
            workspace.read(cx).sidebar_items.iter().any(|item| {
                matches!(item, SidebarItem::Session { session_id, pinned: true, .. } if *session_id == session)
            })
        })
    };
    assert!(!pinned_row(&workspace, cx));
    update(window, &workspace, cx, |state, _, _| {
        let record = state.sessions.get(&session).expect("listed").clone();
        assert!(state.session_management.pins.request(&record, true));
        state.rebuild_sidebar();
    });
    assert!(
        pinned_row(&workspace, cx),
        "the row flips before the server answers"
    );
    update(window, &workspace, cx, |state, window, cx| {
        state.finish_pin(
            key.clone(),
            true,
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        )
    });
    assert!(
        !pinned_row(&workspace, cx),
        "a failed write shows the server's state"
    );
    assert_eq!(
        notice(&workspace, cx).as_deref(),
        Some("Couldn't pin the session"),
        "the failure is announced where assistive technology can read it"
    );
    // A confirmed pin comes from the server's metadata.
    update(window, &workspace, cx, |state, window, cx| {
        let record = state.sessions.get(&session).expect("listed").clone();
        state.session_management.pins.request(&record, true);
        let mut metadata = MetadataFacts::default();
        metadata.overlay.pinned_at = Some(11);
        state.finish_pin(key.clone(), true, Some(Ok(metadata)), window, cx)
    });
    assert!(pinned_row(&workspace, cx));
    assert!(cx.read(|cx| workspace.read(cx).session_management.pins.is_empty()));
}
