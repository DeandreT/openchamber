//! Headless window tests of the command palette through real key dispatch
//! and hit testing: opening and closing, the keys that move the selection,
//! running a row, the focus the palette returns, the mirrored layout, and
//! what stays put when file results arrive, the runtime changes, a project
//! is gone or an IME composition ends.
//! Ranking and grouping are covered next to `build_entries`.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext, ElementId, Entity, EntityInputHandler, FocusHandle, SharedString,
    TestAppContext,
};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::shortcuts::{self as shortcut_schema, Overrides, Platform};

use crate::command_palette::{CommandId, CommandPalette, FileEntry, Origin, SHORTCUT, Target};
use crate::locale::Strings;
use crate::shortcuts;
use crate::sidebar_item::SidebarItem;
use crate::workspace::Workspace;
use crate::workspace_tests;

const SESSION_PREFIX: &str = "command-palette:session:";

fn palette(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Option<Entity<CommandPalette>> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .command_palette
            .as_ref()
            .map(|open| open.palette.clone())
    })
}

fn selected_key(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<SharedString> {
    let palette = palette(workspace, cx).expect("the palette is open");
    cx.read(|cx| palette.read(cx).selected_key())
}

fn row_keys(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<SharedString> {
    let palette = palette(workspace, cx).expect("the palette is open");
    cx.read(|cx| palette.read(cx).row_keys())
}

/// The keys GPUI matches for a binding on this platform.
fn keys(combo: &str) -> String {
    shortcuts::keystrokes(combo, Platform::current()).expect("the binding has keys")
}

/// Focuses the composer and returns its focus handle.
fn focus_composer(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Option<FocusHandle> {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        workspace.update(cx, |workspace, cx| workspace.focus_composer(window, cx));
        window.render_frame(cx);
        window.focused(cx)
    })
    .expect("the workspace window is open")
}

fn press(window: AnyWindowHandle, key: &str, cx: &mut TestAppContext) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
}

fn focused(window: AnyWindowHandle, cx: &mut TestAppContext) -> Option<FocusHandle> {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.focused(cx)
    })
    .expect("the workspace window is open")
}

#[gpui_kit::test]
fn the_shortcut_opens_the_palette_and_escape_returns_the_focus(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    let composer = focus_composer(window, &workspace, cx);
    assert!(composer.is_some(), "the composer holds the focus");
    let default = shortcut_schema::effective_combo(SHORTCUT, &Overrides::new());
    press(window, &keys(&default), cx);
    let open = palette(&workspace, cx).expect("the default shortcut opens the palette");
    let field = cx.read(|cx| open.read(cx).input_focus(cx));
    assert!(
        focused(window, cx) == Some(field),
        "typing goes to the palette's field"
    );
    assert!(
        selected_key(&workspace, cx).is_some(),
        "the first row is selected when the palette opens"
    );
    press(window, "escape", cx);
    assert!(
        palette(&workspace, cx).is_none(),
        "Escape closes the palette"
    );
    assert!(
        focused(window, cx) == composer,
        "the focus goes back to what had it"
    );
}

#[gpui_kit::test]
fn the_shortcut_closes_the_palette_unless_it_is_a_selection_key(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    focus_composer(window, &workspace, cx);
    // Ctrl+P is also "previous row" inside the palette, so the default
    // binding moves the selection there, as in the web client. Any other
    // binding of the shortcut closes the palette.
    let rebound = shortcut_schema::save_override(&Overrides::new(), SHORTCUT, "mod+shift+y", None);
    cx.update(|cx| shortcuts::bind(&rebound, cx));
    press(window, &keys("mod+shift+y"), cx);
    assert!(palette(&workspace, cx).is_some());
    let rows = row_keys(&workspace, cx);
    assert!(rows.len() > 2, "the first screen lists several rows");
    assert_eq!(selected_key(&workspace, cx).as_ref(), rows.first());
    press(window, "ctrl-p", cx);
    assert!(
        palette(&workspace, cx).is_some(),
        "Ctrl+P stays inside the palette"
    );
    assert_eq!(
        selected_key(&workspace, cx).as_ref(),
        rows.last(),
        "Ctrl+P from the first row wraps to the last"
    );
    press(window, "ctrl-n", cx);
    assert_eq!(selected_key(&workspace, cx).as_ref(), rows.first());
    press(window, &keys("mod+shift+y"), cx);
    assert!(
        palette(&workspace, cx).is_none(),
        "the shortcut that opened the palette closes it"
    );
}

#[gpui_kit::test]
fn keys_move_the_selection_and_enter_runs_the_selected_command(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    let composer = focus_composer(window, &workspace, cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_command_palette(window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    let rows = row_keys(&workspace, cx);
    assert_eq!(selected_key(&workspace, cx).as_ref(), rows.first());
    press(window, "down", cx);
    assert_eq!(selected_key(&workspace, cx).as_ref(), rows.get(1));
    press(window, "up", cx);
    press(window, "up", cx);
    assert_eq!(
        selected_key(&workspace, cx).as_ref(),
        rows.last(),
        "Up from the first row wraps to the last"
    );
    press(window, "home", cx);
    assert_eq!(selected_key(&workspace, cx).as_ref(), rows.first());
    press(window, "end", cx);
    assert_eq!(selected_key(&workspace, cx).as_ref(), rows.last());
    press(window, "down", cx);
    assert_eq!(
        selected_key(&workspace, cx).as_ref(),
        rows.first(),
        "Down from the last row wraps to the first"
    );

    let sidebar_open = cx.read(|cx| workspace.read(cx).sidebar_open);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.input("toggle", cx);
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    assert_eq!(
        selected_key(&workspace, cx).as_deref(),
        Some("command-palette:toggle-sidebar"),
        "typing selects the best match"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let row = window.find(ElementId::Name("command-palette:toggle-sidebar".into()));
        assert!(row.visible(), "the matching row is on screen");
        assert_eq!(row.role(), Some(gpui_kit::Role::ListBoxOption));
        assert_eq!(row.selected(), Some(true));
    })
    .expect("the workspace window is open");
    press(window, "enter", cx);
    assert!(
        palette(&workspace, cx).is_none(),
        "running a command closes the palette"
    );
    assert_eq!(
        cx.read(|cx| workspace.read(cx).sidebar_open),
        !sidebar_open,
        "Enter ran Toggle Sidebar"
    );
    assert!(
        focused(window, cx) == composer,
        "a command that takes no focus leaves it where it was"
    );
}

#[gpui_kit::test]
fn a_click_on_a_session_row_opens_that_session(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    focus_composer(window, &workspace, cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_command_palette(window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    let rows = row_keys(&workspace, cx);
    let sessions: Vec<&SharedString> = rows
        .iter()
        .filter(|key| key.starts_with(SESSION_PREFIX))
        .collect();
    assert!(
        !sessions.is_empty() && sessions.len() <= 5,
        "an empty query lists up to five recent sessions"
    );
    let key = sessions[sessions.len() - 1].clone();
    let session = SessionId::from(&key[SESSION_PREFIX.len()..]);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(ElementId::Name(key.clone()), cx);
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    assert!(palette(&workspace, cx).is_none());
    assert_eq!(
        cx.read(|cx| workspace.read(cx).selected.clone()),
        Some(session)
    );
}

/// Opens the palette in `locale` and reports whether the New Session row's
/// shortcut sits left of its title.
fn shortcut_is_left_of_the_title(locale: &str, cx: &mut TestAppContext) -> bool {
    let (window, workspace) = workspace_tests::open(cx);
    cx.update(|cx| cx.set_global(Strings::load(locale)));
    focus_composer(window, &workspace, cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_command_palette(window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let title = window
            .find(ElementId::Name("command-palette:new-session:title".into()))
            .bounds();
        let shortcut = window
            .find(ElementId::Name(
                "command-palette:new-session:shortcut".into(),
            ))
            .bounds();
        assert!(
            shortcut.right() <= title.left() || title.right() <= shortcut.left(),
            "the title and the shortcut never overlap"
        );
        shortcut.right() <= title.left()
    })
    .expect("the workspace window is open")
}

#[gpui_kit::test]
fn the_shortcut_sits_at_the_rows_end_in_english(cx: &mut TestAppContext) {
    assert!(!shortcut_is_left_of_the_title("en", cx));
}

#[gpui_kit::test]
fn rows_mirror_in_a_right_to_left_locale(cx: &mut TestAppContext) {
    assert!(shortcut_is_left_of_the_title("ar", cx));
}

fn open_palette(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Entity<CommandPalette> {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_command_palette(window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    palette(workspace, cx).expect("the palette is open")
}

/// Runs a row's target as if it had been listed for `origin`.
fn run_target(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    origin: &Origin,
    target: Target,
    cx: &mut TestAppContext,
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.run_palette_target(origin, target, window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
}

#[gpui_kit::test]
fn file_results_leave_the_selection_where_it_is(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    focus_composer(window, &workspace, cx);
    let open = open_palette(window, &workspace, cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.input("toggle", cx);
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    press(window, "down", cx);
    let chosen = selected_key(&workspace, cx);
    assert!(chosen.is_some());
    assert_ne!(
        chosen.as_ref(),
        row_keys(&workspace, cx).first(),
        "the user moved off the first row"
    );
    let found = vec![FileEntry {
        path: "/work/app/src/toggle.rs".to_owned(),
        relative_path: "src/toggle.rs".to_owned(),
    }];
    cx.update(|cx| {
        open.update(cx, |palette, cx| {
            palette.apply_file_results("toggle".to_owned(), found, cx)
        })
    });
    assert!(
        row_keys(&workspace, cx)
            .iter()
            .any(|key| key.as_ref() == "command-palette:file:/work/app/src/toggle.rs"),
        "the found file is listed"
    );
    assert_eq!(
        selected_key(&workspace, cx),
        chosen,
        "results arriving keep the selected row"
    );
    // No hits is also what a failed search hands over.
    cx.update(|cx| {
        open.update(cx, |palette, cx| {
            palette.apply_file_results("toggle".to_owned(), Vec::new(), cx)
        })
    });
    assert_eq!(selected_key(&workspace, cx), chosen);
    // Results for a query the field no longer holds change nothing.
    let rows = row_keys(&workspace, cx);
    cx.update(|cx| {
        open.update(cx, |palette, cx| {
            palette.apply_file_results(
                "togg".to_owned(),
                vec![FileEntry {
                    path: "/work/app/src/other.rs".to_owned(),
                    relative_path: "src/other.rs".to_owned(),
                }],
                cx,
            )
        })
    });
    assert_eq!(row_keys(&workspace, cx), rows);
    assert_eq!(selected_key(&workspace, cx), chosen);
}

#[gpui_kit::test]
fn a_runtime_switch_closes_the_palette(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    focus_composer(window, &workspace, cx);
    open_palette(window, &workspace, cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.switch_runtime(RuntimeId::from("another-runtime"), window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    assert!(
        palette(&workspace, cx).is_none(),
        "the palette listed the runtime that was left"
    );
}

#[gpui_kit::test]
fn rows_listed_for_another_runtime_or_directory_do_nothing(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    focus_composer(window, &workspace, cx);
    let origin = cx.read(|cx| workspace.read(cx).palette_origin());
    let sidebar_open = cx.read(|cx| workspace.read(cx).sidebar_open);
    let toggle = || Target::Command(CommandId::ToggleSidebar);
    let other_runtime = Origin {
        runtime: RuntimeId::from("another-runtime"),
        ..origin.clone()
    };
    run_target(window, &workspace, &other_runtime, toggle(), cx);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).sidebar_open),
        sidebar_open,
        "a row from another runtime runs nothing"
    );
    let other_root = Origin {
        root: Some("/somewhere/else".to_owned()),
        ..origin.clone()
    };
    run_target(window, &workspace, &other_root, toggle(), cx);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).sidebar_open),
        sidebar_open,
        "a row from another directory runs nothing"
    );
    run_target(window, &workspace, &origin, toggle(), cx);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).sidebar_open),
        !sidebar_open,
        "a row from the current runtime and directory runs"
    );
}

#[gpui_kit::test]
fn a_project_row_for_a_missing_project_keeps_the_selected_session(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    let (session, project) = cx.read(|cx| {
        let workspace = workspace.read(cx);
        let session = workspace
            .sidebar_items
            .iter()
            .find_map(|item| match item {
                SidebarItem::Session { session_id, .. } => Some(session_id.clone()),
                _ => None,
            })
            .expect("the synthetic sidebar lists a session");
        let project = workspace.topology.projects[0].path.clone();
        (session, project)
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(session.clone(), window, cx)
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    let selected = |cx: &mut TestAppContext| cx.read(|cx| workspace.read(cx).selected.clone());
    assert_eq!(selected(cx), Some(session.clone()));
    // The directory follows the selected session, so the origin is read now.
    let origin = cx.read(|cx| workspace.read(cx).palette_origin());
    let missing = Target::Project("/no/such/project".to_owned());
    run_target(window, &workspace, &origin, missing, cx);
    assert_eq!(
        selected(cx),
        Some(session),
        "a project that is gone leaves the session open"
    );
    run_target(window, &workspace, &origin, Target::Project(project), cx);
    assert_eq!(
        selected(cx),
        None,
        "a registered project opens its new-session draft"
    );
}

#[gpui_kit::test]
fn escape_that_ends_an_ime_composition_keeps_the_palette_open(cx: &mut TestAppContext) {
    let (window, workspace) = workspace_tests::open(cx);
    focus_composer(window, &workspace, cx);
    let open = open_palette(window, &workspace, cx);
    let field = cx.read(|cx| open.read(cx).input().clone());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        field.update(cx, |input, cx| {
            input.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert!(
                input.marked_text_range(window, cx).is_some(),
                "the field holds a composition"
            );
        });
    })
    .expect("the workspace window is open");
    cx.run_until_parked();
    press(window, "escape", cx);
    assert!(
        palette(&workspace, cx).is_some(),
        "Escape ended the composition and nothing else"
    );
    cx.update_window(window, |_, window, cx| {
        field.update(cx, |input, cx| {
            assert!(input.marked_text_range(window, cx).is_none());
        });
    })
    .expect("the workspace window is open");
    press(window, "escape", cx);
    assert!(
        palette(&workspace, cx).is_none(),
        "the next Escape closes the palette"
    );
}
