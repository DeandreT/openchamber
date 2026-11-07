//! The pickers through real keyboard dispatch, typing and hit testing.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, TestAppContext};
use openchamber_native_core::autocomplete::{AutocompleteKind, SkillItem, SnippetItem};
use openchamber_native_core::send_config::{AgentEntry, AgentMode};

use super::Catalog;
use crate::locale::Strings;
use crate::send_catalog::{DirectoryCatalog, Load};
use crate::workspace::Workspace;
use crate::workspace_tests::open;

fn text(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> String {
    cx.read(|cx| workspace.read(cx).composer.read(cx).value().to_string())
}

fn picker(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Option<(AutocompleteKind, String, usize)> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .autocomplete
            .open
            .as_ref()
            .map(|open| (open.kind, open.query.clone(), open.selected))
    })
}

fn agent(name: &str, description: &str) -> AgentEntry {
    AgentEntry {
        name: name.to_owned(),
        display_name: None,
        mode: Some(AgentMode::Subagent),
        hidden: false,
        model: None,
        color: None,
        description: Some(description.to_owned()),
    }
}

/// A synthetic window with its first session selected and focused, with
/// test catalogs for the session's directory.
fn session_window(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let id = workspace.sessions.active()[0].id.clone();
            workspace.select_session(id, window, cx);
            let directory = workspace.config_directory().expect("a session directory");
            let mut catalog = DirectoryCatalog::default();
            catalog.agents = Load::Ready(vec![
                agent("review", "Reviews changes"),
                agent("explore", "Reads the codebase"),
                AgentEntry {
                    mode: Some(AgentMode::Primary),
                    ..agent("build", "Builds")
                },
            ]);
            workspace
                .send_catalog
                .insert_test_directory(directory.clone(), catalog);
            let key = (workspace.runtime.clone(), directory);
            workspace.autocomplete.skills.insert(
                key.clone(),
                Catalog {
                    items: Some(vec![
                        SkillItem {
                            name: "release-notes".to_owned(),
                            project: false,
                            description: Some("Write release notes".to_owned()),
                        },
                        SkillItem {
                            name: "deploy".to_owned(),
                            project: true,
                            description: None,
                        },
                    ]),
                    loaded_at: Some(std::time::Instant::now()),
                    read: None,
                },
            );
            workspace.autocomplete.snippets.insert(
                key,
                Catalog {
                    items: Some(vec![SnippetItem {
                        name: "signature".to_owned(),
                        aliases: vec!["sig".to_owned()],
                        description: None,
                        content: "Thanks".to_owned(),
                        project: false,
                    }]),
                    loaded_at: Some(std::time::Instant::now()),
                    read: None,
                },
            );
            workspace.focus_composer(window, cx);
        });
        window.render_frame(cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    (window, workspace)
}

fn act(
    window: AnyWindowHandle,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App),
) {
    cx.update_window(window, |_, window, cx| {
        f(window, cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    cx.run_until_parked();
}

#[gpui_kit::test]
fn the_palette_filters_navigates_and_writes_the_command(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| window.input("/", cx));
    assert_eq!(
        picker(&workspace, cx).map(|open| open.0),
        Some(AutocompleteKind::Command)
    );
    act(window, cx, |window, cx| window.input("comp", cx));
    assert_eq!(
        picker(&workspace, cx),
        Some((AutocompleteKind::Command, "comp".to_owned(), 0))
    );
    // The arrows wrap over the list; Ctrl+N and Ctrl+P walk it too.
    act(window, cx, |window, cx| window.press("up", cx));
    let last = picker(&workspace, cx).expect("open").2;
    assert!(last > 0, "Up from the first row wraps to the last");
    act(window, cx, |window, cx| window.press("ctrl-n", cx));
    assert_eq!(picker(&workspace, cx).expect("open").2, 0);
    act(window, cx, |window, cx| window.press("ctrl-p", cx));
    assert_eq!(picker(&workspace, cx).expect("open").2, last);
    act(window, cx, |window, cx| window.press("down", cx));
    act(window, cx, |window, cx| window.press("enter", cx));
    assert_eq!(text(&workspace, cx), "/compact ");
    assert_eq!(picker(&workspace, cx), None, "a choice closes the palette");
    cx.read(|cx| {
        assert_eq!(
            workspace.read(cx).composer.read(cx).selected_range(),
            9..9,
            "the caret follows the command"
        );
    });
}

#[gpui_kit::test]
fn escape_closes_and_tab_chooses(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| window.input("/und", cx));
    act(window, cx, |window, cx| window.press("escape", cx));
    assert_eq!(picker(&workspace, cx), None);
    assert_eq!(text(&workspace, cx), "/und", "Escape leaves the text");
    act(window, cx, |window, cx| window.input("o", cx));
    act(window, cx, |window, cx| window.press("tab", cx));
    assert_eq!(text(&workspace, cx), "/undo ");
}

#[gpui_kit::test]
fn agents_complete_once_and_stay_complete(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| window.input("ask @", cx));
    let (kind, _, _) = picker(&workspace, cx).expect("the mention list opens");
    assert_eq!(kind, AutocompleteKind::Mention);
    cx.read(|cx| {
        let rows = workspace.read(cx).picker_rows(cx);
        let crate::composer_autocomplete::PickerRows::Mentions { view, .. } = rows else {
            panic!("mention rows");
        };
        let names: Vec<&str> = view
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        assert_eq!(names, ["explore", "review"], "subagents only, by name");
    });
    act(window, cx, |window, cx| window.input("rev", cx));
    act(window, cx, |window, cx| window.press("enter", cx));
    assert_eq!(text(&workspace, cx), "ask @review ");
    assert_eq!(picker(&workspace, cx), None);
    act(window, cx, |window, cx| window.input("now", cx));
    assert_eq!(
        picker(&workspace, cx),
        None,
        "a completed mention is not a query again"
    );
}

#[gpui_kit::test]
fn a_pasted_mention_opens_nothing(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("see @rev".to_owned()));
        window.press("ctrl-v", cx);
    });
    assert_eq!(text(&workspace, cx), "see @rev");
    assert_eq!(picker(&workspace, cx), None);
}

#[gpui_kit::test]
fn skills_and_snippets_replace_their_token(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| window.input("use $", cx));
    cx.read(|cx| {
        let rows = workspace.read(cx).picker_rows(cx);
        let crate::composer_autocomplete::PickerRows::Skills { rows, .. } = rows else {
            panic!("skill rows");
        };
        let names: Vec<&str> = rows.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["deploy", "release-notes"], "project skills first");
    });
    act(window, cx, |window, cx| window.input("rel", cx));
    act(window, cx, |window, cx| window.press("enter", cx));
    assert_eq!(text(&workspace, cx), "use $release-notes ");
    act(window, cx, |window, cx| window.input("#sig", cx));
    assert_eq!(
        picker(&workspace, cx).map(|open| open.0),
        Some(AutocompleteKind::Snippet)
    );
    act(window, cx, |window, cx| window.press("enter", cx));
    assert_eq!(
        text(&workspace, cx),
        "use $release-notes #sig ",
        "the alias typed is the token written"
    );
}

#[gpui_kit::test]
fn the_pointer_selects_and_chooses_a_row(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| window.input("@", cx));
    act(window, cx, |window, cx| {
        window.hover(ElementId::Name("autocomplete-agent-review".into()), cx)
    });
    assert_eq!(picker(&workspace, cx).expect("open").2, 1, "hover selects");
    act(window, cx, |window, cx| {
        window.click(ElementId::Name("autocomplete-agent-review".into()), cx)
    });
    assert_eq!(text(&workspace, cx), "@review ");
    assert_eq!(picker(&workspace, cx), None);
}

#[gpui_kit::test]
fn a_session_mention_writes_its_link_and_goes_as_context(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    let (id, title) = cx.read(|cx| {
        let state = workspace.read(cx);
        let current = state.selected.clone();
        let other = state
            .sessions
            .active()
            .iter()
            // A `#` or `$` in the title would open its own picker first.
            .find(|session| {
                Some(&session.id) != current.as_ref()
                    && session.parent_id.is_none()
                    && !session.title.contains(['#', '$', '@'])
            })
            .expect("another session");
        (other.id.as_str().to_owned(), other.title.clone())
    });
    act(window, cx, |window, cx| {
        window.input(&format!("@{title}"), cx)
    });
    let index = cx.read(|cx| {
        let rows = workspace.read(cx).picker_rows(cx);
        let crate::composer_autocomplete::PickerRows::Mentions { view, .. } = rows else {
            panic!("mention rows");
        };
        let sessions = view
            .sessions
            .iter()
            .position(|session| session.id == id)
            .expect("the session is listed");
        view.agents.len() + view.recent.len() + view.results.len() + sessions
    });
    act(window, cx, |window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.choose_autocomplete_row(index, window, cx)
        });
    });
    let link = format!("openchamber://session/{id}");
    assert_eq!(text(&workspace, cx), format!("{link} "));
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.session_references_ready(Box::new(|_, _, _| {}), window, cx));
            let owner = workspace.composer_key.clone().expect("owner");
            let draft = workspace.drafts.get(&owner).expect("draft");
            assert_eq!(draft.text, format!("{link} "));
            assert_eq!(draft.content.context.len(), 1);
            assert!(draft.content.context[0].text.contains(&id));
            // Deleting the link takes the reference out of the next send.
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("plain", window, cx);
            });
            assert!(workspace.session_references_ready(Box::new(|_, _, _| {}), window, cx));
            let draft = workspace.drafts.get(&owner).expect("draft");
            assert!(draft.content.context.is_empty());
        });
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_popup_sits_at_the_start_edge_in_both_directions(cx: &mut TestAppContext) {
    let (window, workspace) = session_window(cx);
    act(window, cx, |window, cx| window.input("/", cx));
    let left_to_right = cx.update_window(window, |_, window, _| {
        let popup = window.find("composer-autocomplete").bounds();
        let composer = window.find("composer").bounds();
        assert!(
            popup.bottom() <= composer.top(),
            "the popup rises above the composer"
        );
        (
            popup.left() - composer.left(),
            composer.right() - popup.right(),
        )
    });
    let (start, _) = left_to_right.expect("window is open");
    assert_eq!(start, gpui_kit::px(0.0));
    act(window, cx, |window, cx| {
        cx.set_global(Strings::load("ar"));
        window.refresh();
    });
    let _ = workspace;
    let right_to_left = cx
        .update_window(window, |_, window, _| {
            let popup = window.find("composer-autocomplete").bounds();
            let composer = window.find("composer").bounds();
            composer.right() - popup.right()
        })
        .expect("window is open");
    assert_eq!(
        right_to_left,
        gpui_kit::px(0.0),
        "right to left the popup starts at the right"
    );
}
