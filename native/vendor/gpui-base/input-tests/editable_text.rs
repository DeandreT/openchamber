use gpui::accesskit::{
    Action, ActionData, ActionRequest, Node, NodeId, Role, TextPosition, TextSelection, TreeId,
    TreeUpdate,
};
use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, TestAppContext, Window, div, px,
};
use gpui_component::input::{Input, InputContentType, InputState, Textarea, TextareaState};

struct Fields {
    input: Entity<InputState>,
    textarea: Entity<TextareaState>,
    secret: Option<InputContentType>,
    readonly: bool,
    width: Option<Pixels>,
}

impl Render for Fields {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui::prelude::FluentBuilder as _;
        div()
            .size_full()
            .when_some(self.width, |this, width| this.w(width))
            .child(
                Input::new(&self.input)
                    .aria_label("Entry")
                    .readonly(self.readonly)
                    .when_some(self.secret, |input, kind| input.content_type(kind)),
            )
            .child(Textarea::new(&self.textarea).aria_label("Multiline"))
    }
}

fn tree(window: &mut Window, cx: &mut App) -> TreeUpdate {
    window.draw(cx).clear(cx);
    window
        .accessibility_tree_for_test()
        .expect("active accessibility tree")
        .clone()
}

fn entry<'a>(tree: &'a TreeUpdate, label: &str) -> (NodeId, &'a Node) {
    let (id, node) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some(label))
        .expect("named editor node");
    (*id, node)
}

fn node(tree: &TreeUpdate, id: NodeId) -> &Node {
    &tree
        .nodes
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .expect("referenced node exists")
        .1
}

fn text(tree: &TreeUpdate, editor: &Node) -> String {
    editor
        .children()
        .iter()
        .map(|id| node(tree, *id))
        .filter(|run| run.role() == Role::TextRun)
        .map(|run| {
            let value = run.value().expect("TextRun value");
            assert_eq!(
                run.character_lengths()
                    .iter()
                    .map(|length| usize::from(*length))
                    .sum::<usize>(),
                value.len()
            );
            value
        })
        .collect()
}

#[gpui::test]
fn geometry_tracks_wrapped_carets_resize_and_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let source = "Wide 🦀 narrow iii words wrap here and continue\r\nnext 界e\u{301}\n";
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.set_scale_factor(1.25);
        window.activate_accessibility_for_test();
        Fields {
            input: cx.new(|cx| InputState::new(window, cx)),
            textarea: cx.new(|cx| TextareaState::new(window, cx).default_value(source)),
            secret: None,
            readonly: false,
            width: Some(px(120.)),
        }
    });
    cx.update(|window, cx| {
        let textarea = page.read(cx).textarea.clone();
        textarea.update(cx, |state, cx| state.focus(window, cx));
        let initial = tree(window, cx);
        let (_, editor) = entry(&initial, "Multiline");
        assert_eq!(text(&initial, editor), source);
        let old_position = TextPosition {
            node: editor.children()[0],
            character_index: 1,
        };
        assert!(
            editor.children().len() > 3,
            "narrow editor wraps the first logical line"
        );
        // Compare AccessKit positions with the actual caret's shaped prepaint
        // coordinates, including movement onto continuation rows and scrolling.
        for offset in source
            .char_indices()
            .map(|(offset, _)| offset)
            .chain([source.len()])
        {
            if source[..offset].ends_with('\r') {
                continue;
            }
            textarea.update(cx, |state, cx| state.set_selected_range(offset..offset, cx));
            let snapshot = tree(window, cx);
            let (_, editor) = entry(&snapshot, "Multiline");
            let position = editor.text_selection().expect("caret selection").focus;
            let run = node(&snapshot, position.node);
            let bounds = run.bounds().expect("laid-out caret run bounds");
            let relative_x = if position.character_index == run.character_lengths().len() {
                bounds.width()
            } else {
                f64::from(
                    run.character_positions().expect("character positions")
                        [position.character_index],
                )
            };
            let geometry = textarea
                .read(cx)
                .prepaint_caret_geometry()
                .expect("fresh caret geometry");
            let caret = geometry.cursor_bounds();
            assert!(
                (bounds.x0 + relative_x
                    - f64::from(f32::from(caret.origin.x) * window.scale_factor()))
                .abs()
                    < 0.01,
                "caret x at byte {offset}"
            );
            let caret_line_top = caret.origin.y - (geometry.line_height() - caret.size.height) / 2.
                + geometry.scroll_offset().y;
            assert!(
                (bounds.y0 - f64::from(f32::from(caret_line_top) * window.scale_factor())).abs()
                    < 0.01,
                "caret y at byte {offset}"
            );
            assert_eq!(
                run.character_widths().expect("character widths").len(),
                run.character_lengths().len()
            );
        }
        page.update(cx, |page, cx| {
            page.width = Some(px(360.));
            cx.notify();
        });
        let snapshot = tree(window, cx);
        let (id, editor) = entry(&snapshot, "Multiline");
        assert_ne!(
            editor.children()[0],
            old_position.node,
            "changed run boundaries get new IDs"
        );
        let before = textarea.read(cx).selected_range();
        window.dispatch_accessibility_action_for_test(
            ActionRequest {
                action: Action::SetTextSelection,
                target_tree: TreeId::ROOT,
                target_node: id,
                data: Some(ActionData::SetTextSelection(TextSelection {
                    anchor: old_position,
                    focus: old_position,
                })),
            },
            cx,
        );
        assert_eq!(
            textarea.read(cx).selected_range(),
            before,
            "old wrapped positions cannot select different text after resize"
        );
        let first = editor.children()[0];
        let second = editor.children()[1];
        let first_bytes = node(&snapshot, first).value().expect("first run").len();
        window.dispatch_accessibility_action_for_test(
            ActionRequest {
                action: Action::SetTextSelection,
                target_tree: TreeId::ROOT,
                target_node: id,
                data: Some(ActionData::SetTextSelection(TextSelection {
                    anchor: TextPosition {
                        node: first,
                        character_index: 0,
                    },
                    focus: TextPosition {
                        node: second,
                        character_index: 1,
                    },
                })),
            },
            cx,
        );
        assert_eq!(
            textarea.read(cx).selected_range(),
            0..first_bytes + usize::from(node(&snapshot, second).character_lengths()[0])
        );
    });
}

#[gpui::test]
fn entry_text_selection_and_platform_selection_actions(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        Fields {
            input: cx.new(|cx| InputState::new(window, cx).default_value("a🦀b")),
            textarea: cx.new(|cx| TextareaState::new(window, cx)),
            secret: None,
            readonly: true,
            width: None,
        }
    });
    cx.update(|window, cx| {
        let input = page.read(cx).input.clone();
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.set_selected_range(1..5, cx);
        });
        let snapshot = tree(window, cx);
        let (id, editor) = entry(&snapshot, "Entry");
        assert_eq!(text(&snapshot, editor), "a🦀b");
        assert!(editor.is_read_only());
        assert!(editor.supports_action(Action::SetTextSelection));
        let selection = editor.text_selection().expect("active selection");
        assert_eq!(selection.anchor.character_index, 1);
        assert_eq!(selection.focus.character_index, 2);
        let run = selection.anchor.node;
        let reversed = TextSelection {
            anchor: TextPosition {
                node: run,
                character_index: 3,
            },
            focus: TextPosition {
                node: run,
                character_index: 1,
            },
        };
        window.dispatch_accessibility_action_for_test(
            ActionRequest {
                action: Action::SetTextSelection,
                target_tree: TreeId::ROOT,
                target_node: id,
                data: Some(ActionData::SetTextSelection(reversed)),
            },
            cx,
        );
        assert_eq!(input.read(cx).selected_range(), 1..6);
        let snapshot = tree(window, cx);
        let (_, editor) = entry(&snapshot, "Entry");
        let selection = editor.text_selection().expect("reversed selection");
        assert_eq!(selection.anchor.character_index, 3);
        assert_eq!(selection.focus.character_index, 1);
        for invalid in [
            TextPosition {
                node: run,
                character_index: usize::MAX,
            },
            TextPosition {
                node: NodeId(u64::MAX),
                character_index: 0,
            },
        ] {
            window.dispatch_accessibility_action_for_test(
                ActionRequest {
                    action: Action::SetTextSelection,
                    target_tree: TreeId::ROOT,
                    target_node: id,
                    data: Some(ActionData::SetTextSelection(TextSelection {
                        anchor: invalid,
                        focus: invalid,
                    })),
                },
                cx,
            );
            assert_eq!(input.read(cx).selected_range(), 1..6);
        }
        input.update(cx, |state, cx| state.set_value("updated", window, cx));
        let current_range = input.read(cx).selected_range();
        window.dispatch_accessibility_action_for_test(
            ActionRequest {
                action: Action::SetTextSelection,
                target_tree: TreeId::ROOT,
                target_node: id,
                data: Some(ActionData::SetTextSelection(*selection)),
            },
            cx,
        );
        assert_eq!(
            input.read(cx).selected_range(),
            current_range,
            "stale rendered positions are rejected"
        );
        let snapshot = tree(window, cx);
        assert_eq!(text(&snapshot, entry(&snapshot, "Entry").1), "updated");
        let (current_id, current_editor) = entry(&snapshot, "Entry");
        assert_ne!(
            current_editor
                .text_selection()
                .expect("current caret")
                .anchor
                .node,
            run
        );
        window.dispatch_accessibility_action_for_test(
            ActionRequest {
                action: Action::SetTextSelection,
                target_tree: TreeId::ROOT,
                target_node: current_id,
                data: Some(ActionData::SetTextSelection(*selection)),
            },
            cx,
        );
        assert_eq!(
            input.read(cx).selected_range(),
            current_range,
            "positions from a previous document are rejected after redraw too"
        );
        input.update(cx, |state, cx| state.set_value("a🦀b", window, cx));
        let snapshot = tree(window, cx);
        let (current_id, _) = entry(&snapshot, "Entry");
        let current_range = input.read(cx).selected_range();
        window.dispatch_accessibility_action_for_test(
            ActionRequest {
                action: Action::SetTextSelection,
                target_tree: TreeId::ROOT,
                target_node: current_id,
                data: Some(ActionData::SetTextSelection(*selection)),
            },
            cx,
        );
        assert_eq!(
            input.read(cx).selected_range(),
            current_range,
            "restoring the same text must not revive old positions"
        );
    });
}

#[gpui::test]
fn unlaid_out_text_has_no_geometry_and_scroll_replaces_visible_geometry(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let source = "row with Unicode 界\n".repeat(200);
    let end = source.len();
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        Fields {
            input: cx.new(|cx| InputState::new(window, cx).default_value("x".repeat(600))),
            textarea: cx.new(|cx| TextareaState::new(window, cx).default_value(source)),
            secret: None,
            readonly: false,
            width: Some(px(360.)),
        }
    });
    cx.update(|window, cx| {
        let textarea = page.read(cx).textarea.clone();
        textarea.update(cx, |state, cx| {
            state.focus(window, cx);
            state.set_selected_range(0..0, cx);
        });
        let snapshot = tree(window, cx);
        let (_, editor) = entry(&snapshot, "Multiline");
        assert!(node(&snapshot, editor.children()[0]).bounds().is_some());
        let last = node(&snapshot, *editor.children().last().expect("last run"));
        assert!(
            last.bounds().is_none(),
            "unlaid-out text must not claim a rectangle"
        );
        assert!(last.character_positions().is_none());
        let (_, single) = entry(&snapshot, "Entry");
        let first = node(&snapshot, single.children()[0]);
        let second = node(&snapshot, single.children()[1]);
        assert_eq!(first.character_lengths().len(), 255);
        assert_eq!(first.next_on_line(), Some(single.children()[1]));
        assert_eq!(second.previous_on_line(), Some(single.children()[0]));
        assert_eq!(
            first.bounds().expect("first fragment").y0,
            second.bounds().expect("second fragment").y0
        );
        textarea.update(cx, |state, cx| state.set_selected_range(end..end, cx));
        tree(window, cx);
        let snapshot = tree(window, cx);
        let (_, editor) = entry(&snapshot, "Multiline");
        let last = node(&snapshot, *editor.children().last().expect("last run"));
        assert!(
            last.bounds().is_some(),
            "scrolling lays out the final caret"
        );
        assert!(
            node(&snapshot, editor.children()[0]).bounds().is_none(),
            "old visible geometry is cleared"
        );
    });
}

#[gpui::test]
fn multiline_runs_preserve_long_lines_crlf_and_empty_document(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let source = format!("{}\r\nnext\n", "x".repeat(300));
    let expected = source.clone();
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        Fields {
            input: cx.new(|cx| InputState::new(window, cx)),
            textarea: cx.new(|cx| TextareaState::new(window, cx).default_value(source)),
            secret: None,
            readonly: false,
            width: None,
        }
    });
    cx.update(|window, cx| {
        let textarea = page.read(cx).textarea.clone();
        textarea.update(cx, |state, cx| state.focus(window, cx));
        let snapshot = tree(window, cx);
        let (_, editor) = entry(&snapshot, "Multiline");
        assert_eq!(text(&snapshot, editor), expected);
        let first = node(&snapshot, editor.children()[0]);
        let second = node(&snapshot, editor.children()[1]);
        assert!(first.character_lengths().len() <= 255);
        assert_eq!(first.next_on_line(), None, "soft wrap ends the visual line");
        assert_eq!(second.previous_on_line(), None);
        assert!(
            second.bounds().expect("second visual line").y0
                > first.bounds().expect("first visual line").y0
        );
        assert_eq!(
            second.character_lengths().last(),
            Some(&2),
            "CRLF is one editable character"
        );
        assert_eq!(second.next_on_line(), None);
        textarea.update(cx, |state, cx| state.set_value("", window, cx));
        let snapshot = tree(window, cx);
        let (_, editor) = entry(&snapshot, "Multiline");
        assert_eq!(text(&snapshot, editor), "");
        assert!(node(&snapshot, editor.children()[0]).bounds().is_some());
        assert_eq!(
            editor.children().len(),
            1,
            "empty entries retain the Text interface"
        );
        assert_eq!(
            editor
                .text_selection()
                .expect("empty caret")
                .focus
                .character_index,
            0
        );
    });
}

#[gpui::test]
fn secret_inputs_never_expose_plaintext_runs(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        Fields {
            input: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value("private-value")
                    .masked(true)
            }),
            textarea: cx.new(|cx| TextareaState::new(window, cx)),
            secret: None,
            readonly: false,
            width: None,
        }
    });
    cx.update(|window, cx| {
        let input = page.read(cx).input.clone();
        for kind in [
            None,
            Some(InputContentType::Password),
            Some(InputContentType::NewPassword),
        ] {
            page.update(cx, |page, cx| {
                page.secret = kind;
                cx.notify();
            });
            input.update(cx, |state, cx| {
                state.set_masked(kind.is_none(), window, cx);
                state.focus(window, cx);
            });
            let snapshot = tree(window, cx);
            let (_, editor) = entry(&snapshot, "Entry");
            assert_eq!(editor.value(), None);
            assert!(editor.children().is_empty());
            assert!(!editor.supports_action(Action::SetTextSelection));
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .all(|(_, node)| node.value() != Some("private-value"))
            );
        }
    });
}
