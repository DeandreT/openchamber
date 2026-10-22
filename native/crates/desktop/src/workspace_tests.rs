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
