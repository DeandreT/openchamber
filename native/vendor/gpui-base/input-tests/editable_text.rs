use gpui::accesskit::{
    Action, ActionData, ActionRequest, Node, NodeId, Role, TextPosition, TextSelection, TreeId,
    TreeUpdate,
};
use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, Window, div,
};
use gpui_component::input::{Input, InputContentType, InputState, Textarea, TextareaState};

struct Fields {
    input: Entity<InputState>,
    textarea: Entity<TextareaState>,
    secret: Option<InputContentType>,
    readonly: bool,
}

impl Render for Fields {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui::prelude::FluentBuilder as _;
        div()
            .size_full()
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
fn entry_text_selection_and_platform_selection_actions(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| {
        window.activate_accessibility_for_test();
        Fields {
            input: cx.new(|cx| InputState::new(window, cx).default_value("a🦀b")),
            textarea: cx.new(|cx| TextareaState::new(window, cx)),
            secret: None,
            readonly: true,
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
        assert_eq!(first.character_lengths().len(), 255);
        assert_eq!(first.next_on_line(), Some(editor.children()[1]));
        assert_eq!(second.previous_on_line(), Some(editor.children()[0]));
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
