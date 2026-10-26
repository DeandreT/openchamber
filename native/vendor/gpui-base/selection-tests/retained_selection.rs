use gpui::{
    AppContext as _, Context, Entity, IntoElement, Modifiers, MouseButton, ParentElement as _,
    Render, Styled as _, TestAppContext, Window, div, point, px,
};
use gpui_base::text::{
    MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin, markdown_ast,
};
use gpui_base::text::{
    TextView, TextViewSelectionDocument, TextViewSelectionKey, TextViewSelectionPosition,
    TextViewState,
};
use gpui_base::{TextSelection, TextSelectionLayer};

#[gpui::test]
fn copy_without_a_focused_row_preserves_selected_whitespace(cx: &mut TestAppContext) {
    cx.update(gpui_base::init);
    let (page, cx) = cx.add_window_view(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    let (document, _view) = cx.update(|window, cx| {
        let document = TextViewSelectionDocument::new();
        let view = cx.new(|cx| TextViewState::markdown("alpha beta", cx));
        document
            .attach(TextViewSelectionKey(1), 1, &view, cx)
            .unwrap();
        document.activate(window, cx);
        page.update(cx, |page, cx| {
            page.visible = vec![view.clone()];
            cx.notify();
        });
        let _ = window.draw(cx);
        document.select(position(1, 5), position(1, 6), cx).unwrap();
        page.update(cx, |page, cx| {
            page.visible.clear();
            cx.notify();
        });
        let _ = window.draw(cx);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
        (document, view)
    });
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-c"
    } else {
        "ctrl-c"
    });
    cx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(" ".into())
        );
        document.clear(cx);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    });
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-c"
    } else {
        "ctrl-c"
    });
    cx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("sentinel".into())
        )
    });
}

#[gpui::test]
fn triple_click_selects_the_whole_fragmented_line(cx: &mut TestAppContext) {
    cx.update(gpui_base::init);
    let (page, cx) = cx.add_window_view(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    let (document, _view) = cx.update(|window, cx| {
        let document = TextViewSelectionDocument::new();
        let view = cx.new(|cx| TextViewState::markdown("before `code` after", cx));
        document
            .attach(TextViewSelectionKey(1), 1, &view, cx)
            .unwrap();
        document.activate(window, cx);
        page.update(cx, |page, cx| {
            page.visible = vec![view.clone()];
            cx.notify();
        });
        let _ = window.draw(cx);
        (document, view)
    });
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: point(px(15.), px(10.)),
        modifiers: Modifiers::default(),
        click_count: 3,
        first_mouse: false,
    });
    cx.simulate_mouse_up(
        point(px(15.), px(10.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.update(|_, _| assert_eq!(document.selected_text(), "before code after"));
}

struct Page {
    visible: Vec<Entity<TextViewState>>,
    width: f32,
}
impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(self.width))
            .h_full()
            .flex()
            .flex_col()
            .child(TextSelectionLayer)
            .children(self.visible.iter().map(|view| {
                div()
                    .h(px(60.))
                    .text_size(px(16.))
                    .child(TextView::new(view))
            }))
    }
}

fn position(key: u64, offset: usize) -> TextViewSelectionPosition {
    TextViewSelectionPosition {
        member: TextViewSelectionKey(key),
        offset,
    }
}

struct AtomicBlock {
    text: String,
}
impl MarkdownPlugin for AtomicBlock {
    fn name(&self) -> &str {
        "retained-test-block"
    }
    fn is_block(&self) -> bool {
        true
    }
    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        if matches!(
            node,
            markdown_ast::Node::Heading(_)
                | markdown_ast::Node::List(_)
                | markdown_ast::Node::Code(_)
        ) {
            Some(
                MarkdownNode::new("retained-test-block", ())
                    .text(self.text.clone())
                    .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
            )
        } else {
            None
        }
    }
    fn render(&self, node: &MarkdownNode, _: &mut Window, _: &mut gpui::App) -> impl IntoElement {
        div().child(node.as_text().to_owned())
    }
}

#[gpui::test]
fn custom_heading_list_and_code_children_emit_once(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    cx.update_window(*window, |_, window, cx| {
        gpui_base::init(cx);
        for (source, text, child_text) in [
            ("# α heading", "α heading", "α heading"),
            ("- first\n- second", "first\nsecond", "first second"),
            ("```rust\nlet x = 1;\n```", "let x = 1;", "let x = 1;"),
        ] {
            let doc = TextViewSelectionDocument::new();
            let parent = cx.new(|cx| TextViewState::markdown(source, cx));
            parent.update(cx, |state, cx| {
                state.set_markdown_extensions(
                    std::sync::Arc::new(MarkdownExtensions::default().plugin(AtomicBlock {
                        text: text.to_owned(),
                    })),
                    cx,
                )
            });
            let child = cx.new(|cx| TextViewState::markdown(text, cx));
            let end = cx.new(|cx| TextViewState::markdown("end", cx));
            doc.attach(TextViewSelectionKey(1), 1, &parent, cx).unwrap();
            doc.attach(TextViewSelectionKey(2), 2, &end, cx).unwrap();
            doc.attach_child(
                TextViewSelectionKey(3),
                TextViewSelectionKey(1),
                0..source.len(),
                &child,
                cx,
            )
            .unwrap();
            doc.activate(window, cx);
            doc.select(position(3, 0), position(3, text.len()), cx)
                .unwrap();
            assert_eq!(doc.selected_text(), child_text);
            doc.select(position(1, 0), position(2, 3), cx).unwrap();
            assert_eq!(doc.selected_text(), format!("{text}\n\nend"));
            doc.select(position(3, text.len()), position(2, 3), cx)
                .unwrap();
            assert_eq!(
                doc.selected_text(),
                format!("{text}\n\nend"),
                "crossing the child boundary selects its parent atom once"
            );
            doc.remove(TextViewSelectionKey(1), cx);
            assert!(doc.member(TextViewSelectionKey(3)).is_none());
            assert!(doc.selected_text().is_empty());
        }
    })
    .unwrap();
}

#[gpui::test]
fn activation_before_first_frame_and_scope_reuse(cx: &mut TestAppContext) {
    let doc = TextViewSelectionDocument::new();
    let initial = doc.clone();
    let (_page, cx) = cx.add_window_view(move |window, cx| {
        gpui_base::init(cx);
        let state = cx.new(|cx| TextViewState::markdown("first", cx));
        initial
            .attach(TextViewSelectionKey(1), 1, &state, cx)
            .unwrap();
        initial.activate(window, cx);
        Page {
            visible: vec![state],
            width: 400.,
        }
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
        doc.select(position(1, 0), position(1, 5), cx).unwrap();
        assert_eq!(TextSelection::selected_text(window, cx), "first");
        doc.clear(cx);
        let _ = window.draw(cx);
        assert_eq!(TextSelection::selected_text(window, cx), "");
    });
}

#[gpui::test]
fn offscreen_unicode_copy_and_unpainted_middle(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    cx.update_window(*window, |_, window, cx| {
        gpui_base::init(cx);
        let document = TextViewSelectionDocument::new();
        let first = cx.new(|cx| TextViewState::markdown("Aé中🙂Z", cx));
        let middle = cx.new(|cx| TextViewState::markdown("never painted", cx));
        let last = cx.new(|cx| TextViewState::markdown("Be\u{301}👩‍💻C", cx));
        for (key, view) in [(1, &first), (2, &middle), (3, &last)] {
            document
                .attach(TextViewSelectionKey(key), key, view, cx)
                .unwrap();
        }
        document.activate(window, cx);
        let end = "Be\u{301}👩‍💻".len();
        for (a, b) in [
            (position(1, 1), position(3, end)),
            (position(3, end), position(1, 1)),
        ] {
            document.select(a, b, cx).unwrap();
            assert_eq!(
                TextSelection::selected_text(window, cx),
                "é中🙂Z\n\nnever painted\n\nBe\u{301}👩‍💻"
            );
            assert_eq!(first.read(cx).selected_text(), "é中🙂Z\n");
            assert!(document.select(position(1, 2), b, cx).is_err());
        }
    })
    .unwrap();
}

#[gpui::test]
fn pointer_selection_survives_unmount_reflow_and_remount(cx: &mut TestAppContext) {
    cx.update(|cx| gpui_base::init(cx));
    let (page, cx) = cx.add_window_view(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    let (doc, first, middle, last) = cx.update(|window, cx| {
        let doc = TextViewSelectionDocument::new();
        let first = cx.new(|cx| TextViewState::markdown("alpha beta gamma", cx));
        let middle = cx.new(|cx| TextViewState::markdown("never painted", cx));
        let last = cx.new(|cx| TextViewState::markdown("delta epsilon", cx));
        for (key, state) in [(1, &first), (2, &middle), (3, &last)] {
            doc.attach(TextViewSelectionKey(key), key, state, cx)
                .unwrap();
        }
        doc.activate(window, cx);
        page.update(cx, |page, cx| {
            page.visible = vec![first.clone(), last.clone()];
            cx.notify();
        });
        let _ = window.draw(cx);
        assert!(TextSelection::is_selectable_at(
            point(px(8.), px(8.)),
            window,
            cx
        ));
        (doc, first, middle, last)
    });
    cx.simulate_mouse_down(
        point(px(8.), px(8.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(px(40.), px(68.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(px(40.), px(68.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    let (selected, expected_range) = cx.update(|window, cx| {
        let _ = window.draw(cx);
        let text = TextSelection::selected_text(window, cx);
        assert!(
            text.contains("never painted"),
            "actual pointer selection must include the unpainted middle member: {text:?}"
        );
        (text, first.read(cx).selected_source_range())
    });
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.visible.clear();
            cx.notify();
        });
        let _ = window.draw(cx);
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
        assert_eq!(TextSelection::selected_text(window, cx), selected);
        assert!(
            !TextSelection::is_selectable_at(point(px(8.), px(8.)), window, cx),
            "unmounted hitboxes must be gone"
        );
        first.update(cx, |state, cx| state.invalidate_inline_layout(cx));
        page.update(cx, |page, cx| {
            page.visible = vec![first.clone(), last.clone()];
            page.width = 80.;
            cx.notify();
        });
        let _ = window.draw(cx);
        assert_eq!(TextSelection::selected_text(window, cx), selected);
        assert!(expected_range.is_some());
        assert_eq!(
            first.read(cx).selected_source_range(),
            expected_range,
            "remounted inline must paint the same logical selection after reflow"
        );
        assert_eq!(middle.read(cx).selected_text(), "never painted\n");
        doc.clear(cx);
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
        let other = TextViewSelectionDocument::new();
        other.activate(window, cx);
        let _ = window.draw(cx);
        assert!(
            !TextSelection::is_selectable_at(point(px(8.), px(8.)), window, cx),
            "inactive document's painted text must not accept a gesture"
        );
    });
}

#[gpui::test]
fn default_participant_still_clears_on_unmount(cx: &mut TestAppContext) {
    cx.update(|cx| gpui_base::init(cx));
    let (page, cx) = cx.add_window_view(|_, cx| Page {
        visible: vec![cx.new(|cx| TextViewState::markdown("alpha beta gamma", cx))],
        width: 400.,
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_mouse_down(
        point(px(8.), px(8.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(px(80.), px(8.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(px(80.), px(8.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    let view = cx.update(|window, cx| {
        let _ = window.draw(cx);
        assert!(!TextSelection::selected_text(window, cx).is_empty());
        let view = page.read(cx).visible[0].clone();
        page.update(cx, |page, cx| {
            page.visible.clear();
            cx.notify();
        });
        let _ = window.draw(cx);
        view
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
        assert_eq!(TextSelection::selected_text(window, cx), "");
        assert_eq!(view.read(cx).selected_text(), "");
    });
}

#[gpui::test]
fn append_delete_order_and_isolation(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    cx.update_window(*window, |_, window, cx| {
        gpui_base::init(cx);
        let doc = TextViewSelectionDocument::new();
        let views =
            ["alpha", "middle", "omega"].map(|text| cx.new(|cx| TextViewState::markdown(text, cx)));
        for (i, view) in views.iter().enumerate() {
            doc.attach(TextViewSelectionKey(i as u64), i as u64, view, cx)
                .unwrap();
        }
        doc.activate(window, cx);
        doc.select(position(0, 1), position(2, 3), cx).unwrap();
        let expected = "lpha\n\nmiddle\n\nome";
        assert_eq!(doc.selected_text(), expected);
        doc.set_text(TextViewSelectionKey(2), "omega appended", cx)
            .unwrap();
        assert_eq!(
            doc.selected_text(),
            expected,
            "appending must not move the endpoint"
        );
        doc.remove(TextViewSelectionKey(1), cx);
        assert_eq!(doc.selected_text(), "lpha\n\nome");
        doc.set_order(TextViewSelectionKey(0), 20, cx).unwrap();
        assert_eq!(
            doc.selected_text(),
            "ga appended\n\na",
            "positions follow identity when logical order changes"
        );
        doc.set_order(TextViewSelectionKey(0), 0, cx).unwrap();
        let other = TextViewSelectionDocument::new();
        let other_view = cx.new(|cx| TextViewState::markdown("second session", cx));
        other
            .attach(TextViewSelectionKey(0), 0, &other_view, cx)
            .unwrap();
        other.activate(window, cx);
        assert_eq!(doc.selected_text(), "");
        assert_eq!(TextSelection::selected_text(window, cx), "");
        other.select(position(0, 0), position(0, 6), cx).unwrap();
        doc.select(position(0, 1), position(2, 3), cx).unwrap();
        assert_eq!(doc.selected_text(), "");
        assert_eq!(
            TextSelection::selected_text(window, cx),
            "second",
            "identical keys in an inactive session cannot supply copy text"
        );
        doc.activate(window, cx);
        doc.select(position(0, 1), position(2, 3), cx).unwrap();
        doc.set_text(TextViewSelectionKey(2), "replacement", cx)
            .unwrap();
        assert_eq!(
            doc.selected_text(),
            "",
            "destructive endpoint change invalidates selection synchronously"
        );
        doc.select(position(0, 1), position(2, 3), cx).unwrap();
        doc.remove(TextViewSelectionKey(0), cx);
        assert_eq!(doc.selected_text(), "");
    })
    .unwrap();
}

#[gpui::test]
fn nested_member_is_copied_once(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    cx.update_window(*window, |_, window, cx| {
        gpui_base::init(cx);
        let doc = TextViewSelectionDocument::new();
        let parent = cx.new(|cx| TextViewState::markdown("before\n\n# title\n\nafter", cx));
        let child = cx.new(|cx| TextViewState::markdown("title", cx));
        let end = cx.new(|cx| TextViewState::markdown("end", cx));
        doc.attach(TextViewSelectionKey(1), 1, &parent, cx).unwrap();
        doc.attach(TextViewSelectionKey(2), 2, &end, cx).unwrap();
        doc.attach_child(
            TextViewSelectionKey(3),
            TextViewSelectionKey(1),
            8..15,
            &child,
            cx,
        )
        .unwrap();
        doc.activate(window, cx);
        doc.select(position(3, 1), position(3, 4), cx).unwrap();
        assert_eq!(doc.selected_text(), "itl");
        doc.select(position(1, 0), position(2, 3), cx).unwrap();
        assert_eq!(doc.selected_text(), "before\ntitle\nafter\n\nend");
        doc.select(position(3, 2), position(2, 3), cx).unwrap();
        assert_eq!(doc.selected_text(), "title\nafter\n\nend");
    })
    .unwrap();
}

#[gpui::test]
fn incompatible_markdown_append_and_unrelated_replacement(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    let doc = cx
        .update_window(*window, |_, window, cx| {
            gpui_base::init(cx);
            let doc = TextViewSelectionDocument::new();
            let selected = cx.new(|cx| TextViewState::markdown("*alpha", cx));
            let unrelated = cx.new(|cx| TextViewState::markdown("unrelated", cx));
            doc.attach(TextViewSelectionKey(1), 1, &selected, cx)
                .unwrap();
            doc.attach(TextViewSelectionKey(2), 2, &unrelated, cx)
                .unwrap();
            doc.activate(window, cx);
            doc.select(position(1, 2), position(1, 4), cx).unwrap();
            let before = doc.selected_text();
            assert!(!before.is_empty());
            doc.set_text(TextViewSelectionKey(2), "replaced", cx)
                .unwrap();
            assert_eq!(doc.selected_text(), before);
            doc.set_text(TextViewSelectionKey(1), "*alpha*", cx)
                .unwrap();
            assert_eq!(
                doc.selected_text(),
                "",
                "closing markup changed the rendered prefix"
            );
            doc.select(position(1, 1), position(1, 4), cx).unwrap();
            drop(selected);
            doc
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        doc.selected_text(),
        "",
        "the document must not keep a removed view alive"
    );
    assert!(doc.member(TextViewSelectionKey(1)).is_none());
}

#[gpui::test]
fn drag_can_extend_after_anchor_is_unmounted(cx: &mut TestAppContext) {
    cx.update(|cx| gpui_base::init(cx));
    let (page, cx) = cx.add_window_view(|_, _| Page {
        visible: Vec::new(),
        width: 400.,
    });
    let (doc, first, last) = cx.update(|window, cx| {
        let doc = TextViewSelectionDocument::new();
        let first = cx.new(|cx| TextViewState::markdown("alpha beta gamma", cx));
        let last = cx.new(|cx| TextViewState::markdown("delta epsilon", cx));
        doc.attach(TextViewSelectionKey(1), 1, &first, cx).unwrap();
        doc.attach(TextViewSelectionKey(2), 2, &last, cx).unwrap();
        doc.activate(window, cx);
        page.update(cx, |page, cx| {
            page.visible = vec![first.clone()];
            cx.notify();
        });
        (doc, first, last)
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_mouse_down(
        point(px(8.), px(8.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(px(40.), px(8.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    page.update(cx, |page, cx| {
        page.visible = vec![last.clone()];
        cx.notify();
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
    cx.simulate_mouse_move(
        point(px(40.), px(8.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(px(40.), px(8.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.update(|window, cx| {
        assert!(
            doc.selected_text().contains("gamma"),
            "the offscreen anchor must survive an active drag"
        );
        assert!(!first.read(cx).selected_text().is_empty());
        doc.remove(TextViewSelectionKey(2), cx);
        assert_eq!(TextSelection::selected_text(window, cx), "");
        assert!(
            !TextSelection::is_selectable_at(point(px(8.), px(8.)), window, cx),
            "removal must discard geometry before another frame"
        );
    });
}
