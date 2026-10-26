use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, TestAppContext, Window, div, px,
};
use gpui_component::IndexPath;
use gpui_component::list::{List, ListDelegate, ListItem, ListState};

struct Delegate {
    count: usize,
}

impl ListDelegate for Delegate {
    type Item = ListItem;

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.count
    }

    fn accessibility_label(&self, ix: IndexPath, _: &App) -> Option<SharedString> {
        (ix.row < self.count).then(|| format!("Project {}", ix.row).into())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        (ix.row < self.count).then(|| {
            ListItem::new(ix.row)
                .h(px(30.))
                .child(format!("Project {}", ix.row))
        })
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
    }
}

struct SearchList {
    list: Entity<ListState<Delegate>>,
    other: FocusHandle,
}

impl Render for SearchList {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                List::new(&self.list)
                    .search_placeholder("Search projects")
                    .h(px(200.)),
            )
            .child(
                div()
                    .id("other")
                    .role(Role::Button)
                    .track_focus(&self.other)
                    .size(px(30.)),
            )
    }
}

fn snapshot(window: &mut Window, cx: &mut App) -> gpui::accesskit::TreeUpdate {
    window.draw(cx).clear(cx);
    window
        .accessibility_tree_for_test()
        .expect("active test accessibility tree")
        .clone()
}

fn focus_node(tree: &gpui::accesskit::TreeUpdate) -> &gpui::accesskit::Node {
    &tree
        .nodes
        .iter()
        .find(|(id, _)| *id == tree.focus)
        .expect("focus is present")
        .1
}

fn assert_option(tree: &gpui::accesskit::TreeUpdate, expected: Option<&str>) {
    let editor = focus_node(tree);
    assert_eq!(
        editor.role(),
        Role::TextInput,
        "keyboard focus remains on the real editor"
    );
    assert_eq!(editor.label(), Some("Search projects"));
    let target = editor.active_descendant().map(|target| {
        let node = &tree
            .nodes
            .iter()
            .find(|(id, _)| *id == target)
            .expect("active descendant must be present")
            .1;
        assert_eq!(node.role(), Role::ListItem);
        node.label().expect("named option")
    });
    assert_eq!(target, expected);
    assert_eq!(
        tree.nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::TextInput)
            .count(),
        1
    );
}

#[gpui::test]
fn searchable_list_publishes_real_relation_and_keeps_keyboard_dispatch(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        SearchList {
            list: cx.new(|cx| ListState::new(Delegate { count: 3 }, window, cx).searchable(true)),
            other: cx.focus_handle(),
        }
    });
    cx.update(|window, cx| {
        let list = page.read(cx).list.clone();
        list.update(cx, |list, cx| {
            list.set_selected_index(Some(IndexPath::new(0)), window, cx);
            list.focus(window, cx);
            cx.notify();
        });
        assert_option(&snapshot(window, cx), Some("Project 0"));
    });
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| {
        let list = page.read(cx).list.clone();
        assert_eq!(list.read(cx).selected_index(), Some(IndexPath::new(1)));
        assert!(list.focus_handle(cx).is_focused(window));
        assert_option(&snapshot(window, cx), Some("Project 1"));
        // Removing the selected result must remove the relation even before
        // the owner chooses a replacement index.
        list.update(cx, |list, cx| {
            list.delegate_mut().count = 0;
            cx.notify();
        });
        assert_option(&snapshot(window, cx), None);
        list.update(cx, |list, cx| {
            list.delegate_mut().count = 3;
            list.set_selected_index(None, window, cx);
            cx.notify();
        });
        assert_option(&snapshot(window, cx), None);
        list.update(cx, |list, cx| {
            list.set_selected_index(Some(IndexPath::new(2)), window, cx);
            cx.notify();
        });
        assert_option(&snapshot(window, cx), Some("Project 2"));
        let other = page.read(cx).other.clone();
        other.focus(window, cx);
        let tree = snapshot(window, cx);
        assert_eq!(focus_node(&tree).role(), Role::Button);
        assert!(
            tree.nodes
                .iter()
                .all(|(_, node)| node.active_descendant().is_none())
        );
    });
}

struct Siblings {
    owner: FocusHandle,
    row_first: bool,
    selected: bool,
}

impl Render for Siblings {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui::prelude::FluentBuilder as _;
        let owner = div()
            .id("editor")
            .role(Role::TextInput)
            .track_focus(&self.owner)
            .size(px(30.));
        let row = div()
            .id("row")
            .role(Role::ListItem)
            .aria_label("Option")
            .size(px(30.))
            .when(self.selected, |row| {
                row.aria_active_descendant_for(&self.owner)
            });
        if self.row_first {
            div().child(row).child(owner)
        } else {
            div().child(owner).child(row)
        }
    }
}

#[gpui::test]
fn explicit_relation_resolves_after_both_siblings_render(cx: &mut TestAppContext) {
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        Siblings {
            owner: cx.focus_handle(),
            row_first: true,
            selected: true,
        }
    });
    cx.update(|window, cx| {
        let owner = page.read(cx).owner.clone();
        owner.focus(window, cx);
        for row_first in [true, false] {
            page.update(cx, |page, cx| {
                page.row_first = row_first;
                cx.notify();
            });
            let tree = snapshot(window, cx);
            let target = focus_node(&tree)
                .active_descendant()
                .expect("sibling relation");
            assert!(
                tree.nodes
                    .iter()
                    .any(|(id, node)| *id == target && node.label() == Some("Option"))
            );
        }
        page.update(cx, |page, cx| {
            page.selected = false;
            cx.notify();
        });
        assert_eq!(focus_node(&snapshot(window, cx)).active_descendant(), None);
    });
}
