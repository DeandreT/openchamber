//! The sidebar, rail and header as measured in the reference client at
//! 1280 by 800: row slots, the toolbar and its More menu, the footer, the
//! rail's groups and the header's session controls.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    App, AppContext, Bounds, ElementId, Pixels, SharedString, TestAppContext, Window, point, px,
};
use openchamber_native_core::sidebar_rows::{ActivityKey, SectionSpacing};

use crate::sidebar_item::SidebarItem;

fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), gpui_kit::size(px(width), px(height)))
}

/// A control's rectangle measured from the content root, as the parity
/// harness measures it: the client-side frame may inset the root.
fn placed(window: &mut Window, id: impl Into<String>) -> Bounds<Pixels> {
    // The frame is the same on every side of an untiled window.
    let viewport = window.viewport_size();
    let content = crate::chrome::content_size(window);
    let root = point(
        (viewport.width - content.width) / 2.0,
        (viewport.height - content.height) / 2.0,
    );
    let bounds = window.find(name(id)).bounds();
    Bounds::new(
        point(bounds.origin.x - root.x, bounds.origin.y - root.y),
        bounds.size,
    )
}

/// The content root's width and height.
fn content(window: &mut Window) -> (f32, f32) {
    let size = crate::chrome::content_size(window);
    (f32::from(size.width), f32::from(size.height))
}

/// Opens or closes a page from the toolbar's More menu.
pub(crate) fn click_more_page(window: &mut Window, id: &str, cx: &mut App) {
    window.click(name(crate::sidebar_more_menu::TRIGGER_ID), cx);
    window.render_frame(cx);
    window.click(name(id), cx);
}

#[gpui_kit::test]
fn rows_keep_the_reference_slots_and_gaps(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        window.render_frame(cx);
        let (items, gaps) = {
            let state = workspace.read(cx);
            (state.sidebar_items.clone(), state.sidebar_spacing.clone())
        };
        // The list starts under the 48 px title row and the 40 px toolbar.
        let mut top = 88.0;
        let mut checked = Vec::new();
        for (item, gap) in items.iter().zip(&gaps) {
            let height = match item {
                SidebarItem::Zone { .. } => 28.0,
                SidebarItem::Project { .. } => 30.0,
                SidebarItem::Session { .. } => 32.0,
                // Other rows are not part of the measured set.
                _ => break,
            };
            let slot = height + gap.map_or(0.0, SectionSpacing::pixels);
            if top + slot > 700.0 {
                break;
            }
            assert_eq!(
                placed(window, item.key()),
                rect(10.0, top, 256.0, slot),
                "row `{}` keeps its slot inside the list padding and the thumb gutter",
                item.key()
            );
            checked.push(std::mem::discriminant(item));
            top += slot;
        }
        assert!(
            checked.len() >= 3,
            "the sample sidebar opens with rows of the measured kinds"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_projects_caption_opens_the_projects_with_the_widest_gap(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        window.render_frame(cx);
        let state = workspace.read(cx);
        let caption = state
            .sidebar_items
            .iter()
            .position(|item| item.key() == "caption:projects")
            .expect("the sample sidebar lists zones above its projects");
        assert!(
            matches!(
                state.sidebar_items[caption],
                SidebarItem::Zone {
                    activity: ActivityKey::Projects,
                    collapsed: false,
                    ..
                }
            ),
            "the caption is drawn and folds as a zone header"
        );
        assert_eq!(
            state.sidebar_spacing[caption - 1],
            Some(SectionSpacing::Section),
            "the row above the caption closes with pb-4"
        );
        assert_eq!(
            state.sidebar_spacing[caption], None,
            "the caption and its first project read as one heading"
        );
        assert!(
            matches!(
                state.sidebar_items[caption + 1],
                SidebarItem::Project { .. }
            ),
            "the first project follows the caption"
        );
        let header = state.sidebar_items[caption].clone();
        workspace.update(cx, |state, cx| state.toggle_collapsed(&header, cx));
        window.render_frame(cx);
        let state = workspace.read(cx);
        assert!(
            !state
                .sidebar_items
                .iter()
                .any(|item| matches!(item, SidebarItem::Project { .. })),
            "the folded caption hides every project"
        );
        assert_eq!(
            window.find(name("caption:projects")).expanded(),
            Some(false),
            "the caption reports its disclosure state"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_toolbar_holds_three_controls_and_the_more_menu(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        // The machine policy is unread in a test, which counts as enterprise
        // mode and lists no Integrations line.
        workspace.update(cx, |workspace, _| {
            workspace.enterprise.status = crate::enterprise::PolicyStatus {
                enabled: false,
                unavailable: false,
            };
        });
        window.render_frame(cx);
        // `ms-[3px]` after `px-2.5`, 24 px buttons, `gap-1.5`, centred in
        // the 40 px row under the title row.
        for (id, x) in [
            ("search-sessions", 13.0),
            ("add-project", 43.0),
            ("display-mode", 73.0),
            ("sidebar-more", 103.0),
        ] {
            assert_eq!(
                placed(window, id),
                rect(x, 56.0, 24.0, 24.0),
                "toolbar button `{id}`"
            );
        }
        assert_eq!(
            placed(window, "new-session").origin,
            point(px(72.0), px(12.0)),
            "New session follows the two title buttons"
        );
        for id in [
            "source-board",
            "scheduled-tasks",
            "archive",
            "usage-stats",
            "integrations",
        ] {
            assert!(
                window.try_find(name(id)).is_none(),
                "page `{id}` waits in the More menu"
            );
        }

        window.click(name("sidebar-more"), cx);
        window.render_frame(cx);
        assert_eq!(
            placed(window, "sidebar-more-menu").origin,
            point(px(103.0), px(84.0)),
            "the menu opens 4 px under its trigger, aligned to its start"
        );
        for (index, id) in [
            "source-board",
            "scheduled-tasks",
            "archive",
            "usage-stats",
            "integrations",
        ]
        .into_iter()
        .enumerate()
        {
            let line = placed(window, id);
            assert_eq!(
                (line.origin, line.size.height),
                (point(px(107.0), px(88.0 + 28.0 * index as f32)), px(28.0)),
                "menu line `{id}`"
            );
        }

        window.click(name("archive"), cx);
        window.render_frame(cx);
        assert!(workspace.read(cx).archive_open, "the line opens its page");
        assert!(
            window.try_find(name("sidebar-more-menu")).is_none(),
            "choosing a page closes the menu"
        );

        click_more_page(window, "usage-stats", cx);
        window.render_frame(cx);
        assert!(
            workspace.read(cx).archive_open,
            "a page this client lacks changes nothing"
        );
        assert!(
            window.try_find(name("sidebar-more-menu")).is_some(),
            "a line without a page leaves the menu open"
        );
        window.click(name("archive"), cx);
        window.render_frame(cx);
        assert!(
            !workspace.read(cx).archive_open,
            "the same line closes its page"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_display_menu_opens_from_its_buttons_start(cx: &mut TestAppContext) {
    let (window, _workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        window.render_frame(cx);
        window.click(name("display-mode"), cx);
        window.render_frame(cx);
        assert_eq!(
            placed(window, "display-menu").origin,
            point(px(73.0), px(84.0)),
            "the menu is aligned to the start of its trigger"
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_footer_holds_settings_and_the_instance(cx: &mut TestAppContext) {
    let (window, _workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        window.render_frame(cx);
        let (_, height) = content(window);
        assert_eq!(
            placed(window, "settings"),
            rect(10.0, height - 32.0, 24.0, 24.0),
            "settings sits in the 40 px footer after `px-2.5`"
        );
        let instance = placed(window, "instance-switcher");
        assert_eq!(
            (instance.right(), instance.top(), instance.size.height),
            (px(270.0), px(height - 32.0), px(24.0)),
            "the instance switcher ends the footer"
        );
        for id in ["usage-stats", "shortcuts", "about"] {
            assert!(
                window.try_find(name(id)).is_none(),
                "`{id}` is not a footer button"
            );
        }
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_rail_draws_its_groups_apart(cx: &mut TestAppContext) {
    let (window, _workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        window.render_frame(cx);
        let (width, _) = content(window);
        // 28 px buttons 8 px apart in the 44 px rail, from 8 px under the
        // header; each group opens after a 1 px divider and another gap.
        for (id, y) in [
            ("rail-context", 56.0),
            ("rail-git", 101.0),
            ("rail-diff", 137.0),
            ("rail-walkthrough", 173.0),
            ("rail-files", 218.0),
            ("rail-terminal", 254.0),
            ("rail-browser", 290.0),
            ("rail-notes", 335.0),
            ("rail-configure", 371.0),
        ] {
            assert_eq!(
                placed(window, id),
                rect(width - 36.0, y, 28.0, 28.0),
                "rail button `{id}`"
            );
        }
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_header_shows_the_session_controls_only_with_their_data(cx: &mut TestAppContext) {
    let (window, _workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.activate_accessibility_for_test();
        window.render_frame(cx);
        assert!(
            window
                .try_find(name(crate::header_controls::CONTEXT_USAGE_ID))
                .is_none(),
            "no session is selected, so there is no context fill to show"
        );
        assert!(
            window
                .try_find(name(crate::header_controls::OPEN_IN_ID))
                .is_none(),
            "the sample workspace has no server on this machine to open a folder for"
        );
        // Without them the header ends as before: two 24 px buttons 8 px
        // apart, 12 px before the three 44 px window controls.
        let (width, _) = content(window);
        assert_eq!(
            placed(window, "work-status-toggle"),
            rect(width - 168.0, 12.0, 24.0, 24.0)
        );
        assert_eq!(
            placed(window, "mini-chat"),
            rect(width - 200.0, 12.0, 24.0, 24.0)
        );
    })
    .expect("window is open");
}
