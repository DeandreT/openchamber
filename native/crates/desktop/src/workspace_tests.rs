//! Headless window tests through real hit testing and keyboard dispatch.
//!
//! These cover the stage-1 behaviors GPUI Kit's test platform can establish:
//! selection, draft preservation, submit/newline keys, stream playback,
//! anchor restoration, keyboard navigation and shutdown during playback.
//! IME, Narrator/AT-SPI announcements and DPI changes still need real-window
//! checks.

use std::time::Duration;

use gpui_kit::test::{TestAppContextExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, ElementId, Entity, ScrollDelta, SharedString,
    TestAppContext, WindowBounds, WindowOptions, point, px, size,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::synthetic::{ConversationShape, ResponseScript, WorkloadSize};

use crate::locale::Strings;
use crate::sidebar_item::SidebarItem;
use crate::theme::{self, Appearance, Palette};
use crate::workspace::{Source, Workspace};
use crate::workspace_view;

fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(Strings::load("en"));
        theme::apply(Palette::default_for(Appearance::Dark), cx);
        workspace_view::bind_keys(cx);
        let (window, workspace) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(1280.0), px(800.0)),
                })),
                ..WindowOptions::default()
            },
            cx,
            |window, cx| {
                window.set_layout_mode(gpui_kit::LayoutMode::Logical);
                cx.new(|cx| Workspace::new(Source::Synthetic(WorkloadSize::Small), window, cx))
            },
        )
        .expect("the test window opens");
        (window, workspace)
    })
}

/// Visible session rows as (row key, session ID), in display order.
fn session_rows(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Vec<(SharedString, SessionId)> {
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

fn id(key: &SharedString) -> ElementId {
    ElementId::Name(key.clone())
}

fn composer_text(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> String {
    cx.read(|cx| workspace.read(cx).composer.read(cx).value().to_string())
}

fn selected(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<SessionId> {
    cx.read(|cx| workspace.read(cx).selected.clone())
}

fn transcript_len(
    workspace: &Entity<Workspace>,
    session: &SessionId,
    cx: &mut TestAppContext,
) -> usize {
    cx.read(|cx| {
        workspace
            .read(cx)
            .open_sessions
            .get(session)
            .map_or(0, |open| open.transcript.len())
    })
}

#[gpui_kit::test]
fn project_chooser_pointer_selects_a_filtered_project_in_rtl(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let directory = cx.read(|cx| workspace.read(cx).topology.projects[1].path.clone());
    cx.update_window(window, |_, window, cx| {
        cx.set_global(Strings::load("ar"));
        theme::apply(Palette::default_for(Appearance::Light), cx);
        workspace.update(cx, |workspace, _| {
            workspace.send_catalog.settings =
                Some(openchamber_native_transport::client::ServerSettings {
                    project_labels: vec![openchamber_native_transport::client::ProjectLabel {
                        path: directory.clone(),
                        label: "Custom project label".to_owned(),
                    }],
                    ..Default::default()
                });
        });
        window.render_frame(cx);
        window.click(ElementId::Name("choose-project".into()), cx);
        window.render_frame(cx);
        window.input("Custom project", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            ElementId::Name(format!("project-option:{directory}").into()),
            cx,
        );
    })
    .expect("window is open");
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| workspace.read(cx).config_directory()),
        Some(directory)
    );
    cx.read(|cx| {
        let state = workspace.read(cx);
        assert_eq!(
            state.draft_project_label().as_deref(),
            Some("Custom project label")
        );
        assert_eq!(
            state
                .header_project_meta()
                .and_then(|metadata| metadata.project)
                .as_deref(),
            Some("Custom project label")
        );
    });
}

#[gpui_kit::test]
fn project_chooser_pointer_opens_search_and_keyboard_selects(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(ElementId::Name("choose-project".into()), cx);
        window.render_frame(cx);
        window.press("ctrl-n", cx);
        window.press("enter", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(
            workspace.draft_projects.contains_key(&workspace.runtime),
            "choosing a popup row commits its directory"
        );
        assert!(workspace.selected.is_none());
    });
    cx.update_window(window, |_, window, cx| {
        window.input("after choosing", cx);
    })
    .expect("window is open");
    assert_eq!(
        composer_text(&workspace, cx),
        "after choosing",
        "selection returns focus to the composer"
    );
    cx.update_window(window, |_, window, cx| {
        window.press("ctrl-a", cx);
    })
    .expect("window is open");
    cx.read(|cx| {
        assert_eq!(
            workspace
                .read(cx)
                .composer
                .read(cx)
                .selected_text()
                .to_string(),
            "after choosing"
        );
    });
}

#[gpui_kit::test]
fn project_chooser_preserves_directory_drafts_and_catalog_scope(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        let projects = workspace.read(cx).topology.projects.clone();
        assert!(projects.len() >= 2, "the fixture has separate projects");
        let first = projects[0].path.clone();
        let second = projects[1].path.clone();
        let sessions = workspace.read(cx).sessions.active().len();
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&first, window, cx)
        });
        window.render_frame(cx);
        window.input("first project draft", cx);
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&second, window, cx)
        });
        assert_eq!(workspace.read(cx).composer.read(cx).value().as_ref(), "");
        assert_eq!(
            workspace.read(cx).config_directory().as_deref(),
            Some(second.as_str())
        );
        window.input("second project draft", cx);
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&first, window, cx)
        });
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "first project draft"
        );
        assert_eq!(
            workspace
                .read(cx)
                .composer_key
                .as_ref()
                .expect("draft key")
                .directory(),
            first
        );
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project("/unregistered", window, cx)
        });
        assert_eq!(
            workspace.read(cx).config_directory().as_deref(),
            Some(first.as_str())
        );
        assert!(workspace.read(cx).selected.is_none());
        assert_eq!(
            workspace.read(cx).sessions.active().len(),
            sessions,
            "selection creates no server session"
        );
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&second, window, cx)
        });
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "second project draft"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn project_chooser_reselection_returns_to_the_composer(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let directory = cx.read(|cx| workspace.read(cx).topology.projects[0].path.clone());
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&directory, window, cx)
        });
        window.render_frame(cx);
        window.input("kept", cx);
        window.click(ElementId::Name("choose-project".into()), cx);
        window.render_frame(cx);
        window.press("enter", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.input(" after selection", cx);
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "kept after selection");
}

#[gpui_kit::test]
fn sidebar_drafts_reopen_and_discard_only_their_own_text(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let directories = cx.read(|cx| {
        workspace
            .read(cx)
            .topology
            .projects
            .iter()
            .take(2)
            .map(|project| project.path.clone())
            .collect::<Vec<_>>()
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&directories[0], window, cx)
        });
        window.input("First line\nsecond line", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    let first = cx.read(|cx| {
        workspace
            .read(cx)
            .sidebar_items
            .iter()
            .find_map(|item| match item {
                SidebarItem::Draft { draft, title, .. } if title.as_ref() == "First line" => {
                    Some(draft.clone())
                }
                _ => None,
            })
            .expect("the first nonempty line is shown in the sidebar")
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&directories[1], window, cx)
        });
        window.input("Other project", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    let second = cx.read(|cx| {
        workspace
            .read(cx)
            .composer_key
            .clone()
            .expect("second draft has a key")
    });
    let session = session_rows(&workspace, cx)[0].1.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(session, window, cx);
            workspace.open_draft(&first, window, cx);
        });
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "First line\nsecond line");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.discard_draft(&second, window, cx)
        });
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "First line\nsecond line");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.discard_draft(&first, window, cx)
        });
    })
    .expect("window is open");
    cx.run_until_parked();
    assert_eq!(composer_text(&workspace, cx), "");
    cx.read(|cx| {
        assert!(
            !workspace
                .read(cx)
                .sidebar_items
                .iter()
                .any(|item| matches!(item, SidebarItem::Draft { .. }))
        )
    });
}

#[gpui_kit::test]
fn project_chooser_isolates_the_same_directory_between_runtimes(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        let directory = workspace.read(cx).topology.projects[0].path.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.choose_draft_project(&directory, window, cx)
        });
        window.render_frame(cx);
        window.input("original runtime draft", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        let directory = workspace.read(cx).topology.projects[0].path.clone();
        let original = workspace.read(cx).runtime.clone();
        workspace.update(cx, |workspace, cx| {
            let projects = workspace.topology.projects.clone();
            workspace.switch_runtime("another-runtime".into(), window, cx);
            workspace.topology.projects = projects;
            workspace.choose_draft_project(&directory, window, cx);
        });
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "",
            "the new runtime cannot inherit old text"
        );
        window.input("another runtime draft", cx);
        workspace.update(cx, |workspace, cx| {
            let projects = workspace.topology.projects.clone();
            workspace.switch_runtime(original, window, cx);
            workspace.topology.projects = projects;
        });
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "original runtime draft"
        );
        assert_eq!(
            workspace.read(cx).config_directory().as_deref(),
            Some(directory.as_str())
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn editing_a_hidden_draft_preserves_project_collapse(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        let project = workspace.read(cx).topology.projects[0].clone();
        workspace.update(cx, |state, cx| {
            state.choose_draft_project(&project.path, window, cx);
            state.sidebar_state.collapsed_projects.insert(project.id);
            state.rebuild_sidebar();
        });
        window.input("latest preview", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    cx.read(|cx| {
        assert!(
            !workspace
                .read(cx)
                .sidebar_items
                .iter()
                .any(|item| matches!(item, SidebarItem::Draft { .. }))
        )
    });
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |state, cx| {
            let id = &state.topology.projects[0].id;
            let header = state.sidebar_items.iter().find(|item| matches!(item, SidebarItem::Project { project_id, .. } if project_id == id)).expect("project header exists").clone();
            state.toggle_collapsed(&header, cx);
        });
    }).expect("window is open");
    cx.read(|cx| assert!(workspace.read(cx).sidebar_items.iter().any(|item| matches!(item, SidebarItem::Draft { title, .. } if title.as_ref() == "latest preview"))));
}

#[gpui_kit::test]
fn draft_discard_is_reachable_from_the_keyboard(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        let directory = workspace.read(cx).topology.projects[0].path.clone();
        workspace.update(cx, |state, cx| {
            state.choose_draft_project(&directory, window, cx)
        });
        window.input("keyboard draft", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        let key = workspace
            .read(cx)
            .sidebar_items
            .iter()
            .find_map(|item| match item {
                SidebarItem::Draft { key, .. } => Some(key.clone()),
                _ => None,
            })
            .expect("draft row is visible");
        let discard = ElementId::Name(format!("{key}:discard").into());
        for _ in 0..50 {
            window.press("tab", cx);
            window.render_frame(cx);
            if window.find(discard.clone()).focused() == Some(true) {
                window.press("space", cx);
                break;
            }
        }
    })
    .expect("window is open");
    cx.run_until_parked();
    assert_eq!(
        composer_text(&workspace, cx),
        "",
        "Space discards the focused draft"
    );
}

#[gpui_kit::test]
fn draft_preview_edits_preserve_unrelated_sidebar_rows(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        let directory = workspace.read(cx).topology.projects[0].path.clone();
        workspace.update(cx, |state, cx| {
            state.choose_draft_project(&directory, window, cx)
        });
        window.input("First", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    let before = cx.read(|cx| {
        workspace
            .read(cx)
            .sidebar_items
            .iter()
            .filter(|item| !matches!(item, SidebarItem::Draft { .. }))
            .cloned()
            .collect::<Vec<_>>()
    });
    cx.update_window(window, |_, window, cx| window.input(" changed", cx))
        .expect("window is open");
    cx.run_until_parked();
    cx.read(|cx| {
        let state = workspace.read(cx);
        let unrelated = state
            .sidebar_items
            .iter()
            .filter(|item| !matches!(item, SidebarItem::Draft { .. }))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(unrelated, before);
        let drafts: Vec<_> = state
            .sidebar_items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Draft { title, .. } => Some(title.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(drafts, ["First changed"]);
    });
}

#[gpui_kit::test]
fn stale_draft_actions_cannot_touch_another_runtime(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let old = cx
        .update_window(window, |_, window, cx| {
            let directory = workspace.read(cx).topology.projects[0].path.clone();
            workspace.update(cx, |state, cx| {
                state.choose_draft_project(&directory, window, cx)
            });
            window.input("old runtime", cx);
            workspace
                .read(cx)
                .composer_key
                .clone()
                .expect("draft key exists")
        })
        .expect("window is open");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |state, cx| {
            state.switch_runtime("other-runtime".into(), window, cx);
        });
        window.input("new runtime", cx);
        workspace.update(cx, |state, cx| {
            state.discard_draft(&old, window, cx);
            state.open_draft(&old, window, cx);
        });
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "new runtime");
}

#[gpui_kit::test]
fn an_unassigned_draft_does_not_alias_a_project_at_filesystem_root(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        window.input("unassigned text", cx);
        let unassigned = workspace
            .read(cx)
            .composer_key
            .clone()
            .expect("unassigned draft has a key");
        workspace.update(cx, |state, cx| {
            state
                .topology
                .projects
                .push(openchamber_native_core::ownership::ConfiguredProject {
                    id: "root-project".into(),
                    path: "/".to_owned(),
                });
            state.choose_draft_project("/", window, cx);
        });
        assert_ne!(workspace.read(cx).composer_key.as_ref(), Some(&unassigned));
        assert_eq!(workspace.read(cx).composer.read(cx).value().as_ref(), "");
        window.input("root project text", cx);
        workspace.update(cx, |state, cx| {
            state.save_composer_draft(cx);
            state.draft_projects.remove(&state.runtime);
            state.composer_key = state.new_session_draft_key();
            state.restore_composer_draft(window, cx);
        });
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "unassigned text"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn clicking_a_row_selects_its_session(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    let (key, session) = rows[1].clone();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&key), cx);
        assert_eq!(window.find(id(&key)).selected(), Some(true));
    })
    .expect("window is open");
    assert_eq!(selected(&workspace, cx), Some(session.clone()));
    assert!(transcript_len(&workspace, &session, cx) > 0);
}

#[gpui_kit::test]
fn multiline_unicode_paste_is_one_undo_step(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (row, _) = session_rows(&workspace, cx)[0].clone();
    let text = "First e\u{301} line\nمرحبا 你好 👩‍💻\nLast line";
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&row), cx);
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.to_owned()));
        window.press("ctrl-v", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    assert_eq!(composer_text(&workspace, cx), text);
    cx.update_window(window, |_, window, cx| window.press("ctrl-z", cx))
        .expect("window is open");
    cx.run_until_parked();
    assert_eq!(
        composer_text(&workspace, cx),
        "",
        "one undo removes the whole paste"
    );
}

#[gpui_kit::test]
fn narrow_windows_offer_work_status_as_an_overlay(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        assert!(workspace.read(cx).work_status_inline(window));
        workspace.update(cx, |workspace, _| workspace.initial_size = None);
    })
    .expect("window is open");
    cx.simulate_window_resize(window, size(px(900.0), px(600.0)));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            !workspace.read(cx).work_status_inline(window),
            "the panel leaves room for the transcript"
        );
        window.click(ElementId::Name("work-status-toggle".into()), cx);
        assert!(
            workspace.read(cx).work_status_overlay,
            "the header opens the overlay"
        );
        window.render_frame(cx);
        window.click(ElementId::Name("work-status-toggle".into()), cx);
        assert!(
            !workspace.read(cx).work_status_overlay,
            "the same control closes it"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn prompt_navigator_shortcut_preserves_the_draft_and_jumps_to_a_prompt(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let (key, session) = session_rows(&workspace, cx)[1].clone();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&key), cx);
        window.input("keep this draft", cx);
        window.press("ctrl-k", cx);
        window.press("n", cx);
    })
    .expect("window is open");
    let prompt_ids = cx.read(|cx| {
        workspace.read(cx).open_sessions[&session]
            .transcript
            .messages()
            .iter()
            .filter(|message| message.info.role == openchamber_native_core::transcript::Role::User)
            .map(|message| message.info.id.clone())
            .collect::<Vec<_>>()
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(ElementId::Name("prompt-list".into())).visible());
        window.press("ctrl-p", cx);
        window.render_frame(cx);
        let previous = &prompt_ids[prompt_ids.len() - 2];
        assert_eq!(
            window
                .find(ElementId::Name(format!("prompt-row-{previous}").into()))
                .selected(),
            Some(true)
        );
        window.press("ctrl-n", cx);
        window.render_frame(cx);
        let last = prompt_ids.last().expect("prompts exist");
        assert_eq!(
            window
                .find(ElementId::Name(format!("prompt-row-{last}").into()))
                .selected(),
            Some(true)
        );
        window.press("home", cx);
        window.press("enter", cx);
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "keep this draft");
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(
            workspace.prompt_panel.is_none(),
            "choosing a prompt closes the panel"
        );
        let open = &workspace.open_sessions[&session];
        assert_eq!(
            open.list.logical_scroll_top().item_ix,
            0,
            "Home selects the first prompt"
        );
    });
}

#[gpui_kit::test]
fn the_window_opens_on_a_new_session(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    assert_eq!(selected(&workspace, cx), None, "nothing is selected");
    let zone = cx.read(|cx| {
        workspace
            .read(cx)
            .sidebar_items
            .iter()
            .find_map(|item| match item {
                SidebarItem::Zone { key, .. } => Some(key.clone()),
                _ => None,
            })
            .expect("the synthetic sidebar has a zone")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(id(&"new-session".into())).visible());
        assert_eq!(
            window.find(id(&zone)).expanded(),
            Some(true),
            "zone headers report their disclosure state"
        );
    })
    .expect("window is open");
    assert!(
        cx.read(|cx| workspace.read(cx).composer_key.is_some()),
        "the new session's draft has a key"
    );
}

#[gpui_kit::test]
fn new_session_keeps_both_drafts(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    let (session_row, _) = rows[1].clone();
    let new_session = id(&"new-session".into());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.input("a new idea", cx);
        window.click(id(&session_row), cx);
        window.input("a reply", cx);
        window.click(new_session.clone(), cx);
    })
    .expect("window is open");
    assert_eq!(selected(&workspace, cx), None);
    assert_eq!(composer_text(&workspace, cx), "a new idea");

    cx.update_window(window, |_, window, cx| window.click(id(&session_row), cx))
        .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "a reply");
}

#[gpui_kit::test]
fn a_draft_survives_switching_away_and_back(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    let (first, _) = rows[1].clone();
    let (second, _) = rows[2].clone();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&first), cx);
        window.input("unsent — مسودة 👩‍💻", cx);
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "unsent — مسودة 👩‍💻");

    cx.update_window(window, |_, window, cx| window.click(id(&second), cx))
        .expect("window is open");
    assert_eq!(
        composer_text(&workspace, cx),
        "",
        "the other session has no draft"
    );

    cx.update_window(window, |_, window, cx| window.click(id(&first), cx))
        .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "unsent — مسودة 👩‍💻");
}

#[gpui_kit::test]
async fn enter_sends_and_the_stream_plays_to_completion(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    let (key, session) = rows[1].clone();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&key), cx);
    })
    .expect("window is open");
    let before = transcript_len(&workspace, &session, cx);

    cx.update_window(window, |_, window, cx| {
        window.input("first line", cx);
        window.press("shift-enter", cx);
        window.input("second line", cx);
    })
    .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "first line\nsecond line");
    assert_eq!(
        transcript_len(&workspace, &session, cx),
        before,
        "shift-enter only adds a line"
    );

    cx.update_window(window, |_, window, cx| window.press("enter", cx))
        .expect("window is open");
    assert_eq!(composer_text(&workspace, cx), "");
    assert_eq!(transcript_len(&workspace, &session, cx), before + 2);

    let probe = workspace.clone();
    let streaming_session = session.clone();
    cx.wait_for(window, Duration::from_secs(10), move |_, cx| {
        !probe.read(cx).is_streaming(&streaming_session)
    })
    .await;
    let reply = cx.read(|cx| {
        let open = &workspace.read(cx).open_sessions[&session];
        open.transcript
            .message(open.transcript.len() - 1)
            .map(|message| message.plain_text())
            .unwrap_or_default()
    });
    assert_eq!(reply, ResponseScript::new(1).text());
}

#[gpui_kit::test]
fn returning_restores_the_exact_scroll_anchor(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    let rich = rows
        .iter()
        .find(|(_, session)| ConversationShape::for_session(session) == ConversationShape::Rich)
        .cloned()
        .expect("the small workload lists a rich conversation");
    let other = rows
        .iter()
        .find(|(_, session)| *session != rich.1)
        .cloned()
        .expect("the small workload lists another session");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&rich.0), cx);
        window.render_frame(cx);
        window.scroll(
            "transcript",
            ScrollDelta::Pixels(point(px(0.0), px(1_337.0))),
            cx,
        );
        window.render_frame(cx);
    })
    .expect("window is open");
    let anchor = cx.read(|cx| {
        workspace.read(cx).open_sessions[&rich.1]
            .list
            .logical_scroll_top()
    });
    assert!(
        anchor.offset_in_item > px(0.0) || anchor.item_ix > 0,
        "the wheel moved the reader away from the tail"
    );

    cx.update_window(window, |_, window, cx| {
        window.click(id(&other.0), cx);
        window.render_frame(cx);
        window.click(id(&rich.0), cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    let restored = cx.read(|cx| {
        workspace.read(cx).open_sessions[&rich.1]
            .list
            .logical_scroll_top()
    });
    assert_eq!(restored.item_ix, anchor.item_ix);
    assert_eq!(restored.offset_in_item, anchor.offset_in_item);
}

#[gpui_kit::test]
fn alt_down_selects_the_next_session_row(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&rows[0].0), cx);
        window.press("alt-down", cx);
    })
    .expect("window is open");
    assert_eq!(selected(&workspace, cx), Some(rows[1].1.clone()));
}

#[gpui_kit::test]
fn prepending_history_preserves_the_visible_message_and_offset(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rich = session_rows(&workspace, cx)
        .into_iter()
        .find(|(_, session)| ConversationShape::for_session(session) == ConversationShape::Rich)
        .expect("a rich conversation");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&rich.0), cx);
        window.render_frame(cx);
        window.scroll(
            "transcript",
            ScrollDelta::Pixels(point(px(0.0), px(1337.0))),
            cx,
        );
        window.render_frame(cx);
    })
    .expect("window is open");
    let before = cx.read(|cx| {
        let open = &workspace.read(cx).open_sessions[&rich.1];
        let anchor = open.list.logical_scroll_top();
        (
            open.transcript.messages()[anchor.item_ix].info.id.clone(),
            anchor.offset_in_item,
        )
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let open = workspace
                .open_sessions
                .get_mut(&rich.1)
                .expect("session is open");
            let mut older = open.transcript.messages()[0].clone();
            older.info.id = "older-test-message".into();
            let revision = open.transcript.revision();
            let change = open.transcript.prepend_page(vec![older], revision);
            crate::server_sync::apply_list_change(open, &change, cx);
            cx.notify();
        });
        window.render_frame(cx);
    })
    .expect("window is open");
    cx.read(|cx| {
        let open = &workspace.read(cx).open_sessions[&rich.1];
        let anchor = open.list.logical_scroll_top();
        assert_eq!(open.transcript.messages()[anchor.item_ix].info.id, before.0);
        assert_eq!(anchor.offset_in_item, before.1);
    });
}

#[gpui_kit::test]
async fn closing_during_playback_releases_the_workspace(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let rows = session_rows(&workspace, cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(&rows[1].0), cx);
        window.input("stream please", cx);
        window.press("enter", cx);
    })
    .expect("window is open");
    assert!(cx.read(|cx| workspace.read(cx).is_streaming(&rows[1].1)));
    let weak = workspace.downgrade();
    drop(workspace);
    cx.update_window(window, |_, window, _| window.remove_window())
        .expect("window is open");
    cx.run_until_parked();
    assert!(
        weak.upgrade().is_none(),
        "the workspace and its playback task were released"
    );
}
