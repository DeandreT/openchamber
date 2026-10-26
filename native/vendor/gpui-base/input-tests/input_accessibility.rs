use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, Window, div,
};
use gpui_base::test_support::{self, ElementSnapshot};
use gpui_component::input::{Input, InputContentType, InputState, Textarea, TextareaState};

struct Inputs {
    input: Entity<InputState>,
    textarea: Entity<TextareaState>,
    password: bool,
    readonly: bool,
    disabled: bool,
}

impl Render for Inputs {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui::prelude::FluentBuilder as _;
        div()
            .size_full()
            .child(
                Input::new(&self.input)
                    .aria_label("Search")
                    .accessibility_id("search.query")
                    .readonly(self.readonly)
                    .disabled(self.disabled)
                    .when(self.password, |input| {
                        input.content_type(InputContentType::Password)
                    }),
            )
            .child(Textarea::new(&self.textarea).aria_label("Message"))
    }
}

fn tree(window: &mut Window, cx: &mut gpui::App) -> Vec<ElementSnapshot> {
    window.draw(cx).clear(cx);
    test_support::snapshots(window)
}

fn focused(tree: &[ElementSnapshot]) -> &ElementSnapshot {
    let mut focused = tree
        .iter()
        .filter(|node| node.role().is_some() && node.focused() == Some(true));
    let node = focused.next().expect("focused accessible input");
    assert!(
        focused.next().is_none(),
        "only one accessible control owns focus"
    );
    node
}

#[gpui::test]
fn input_and_textarea_focus_own_the_accessible_value(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| Inputs {
        input: cx.new(|cx| InputState::new(window, cx).default_value("query")),
        textarea: cx.new(|cx| TextareaState::new(window, cx).default_value("draft\nnext")),
        password: false,
        readonly: false,
        disabled: false,
    });
    cx.update(|window, cx| {
        let input = page.read(cx).input.clone();
        let textarea = page.read(cx).textarea.clone();
        input.update(cx, |state, cx| state.focus(window, cx));
        let snapshot = tree(window, cx);
        assert_eq!(focused(&snapshot).role(), Some(gpui::Role::TextInput));
        assert_eq!(focused(&snapshot).label(), Some("Search"));
        assert_eq!(focused(&snapshot).value(), Some("query"));
        let entries = snapshot
            .iter()
            .filter(|node| {
                matches!(
                    node.role(),
                    Some(gpui::Role::TextInput | gpui::Role::MultilineTextInput)
                )
            })
            .count();
        assert_eq!(entries, 2, "each control has exactly one editable node");
        textarea.update(cx, |state, cx| state.focus(window, cx));
        let snapshot = tree(window, cx);
        assert_eq!(
            focused(&snapshot).role(),
            Some(gpui::Role::MultilineTextInput)
        );
        assert_eq!(focused(&snapshot).label(), Some("Message"));
        assert_eq!(focused(&snapshot).value(), Some("draft\nnext"));
    });
    cx.simulate_keystrokes("x");
    assert!(page.read_with(cx, |page, cx| page.textarea.read(cx).value().contains('x')));
}

#[gpui::test]
fn focused_secret_inputs_never_publish_their_value(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| Inputs {
        input: cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("private-value")
                .masked(true)
        }),
        textarea: cx.new(|cx| TextareaState::new(window, cx)),
        password: false,
        readonly: false,
        disabled: false,
    });
    cx.update(|window, cx| {
        let input = page.read(cx).input.clone();
        input.update(cx, |state, cx| state.focus(window, cx));
        let snapshot = tree(window, cx);
        assert_eq!(focused(&snapshot).value(), None);
        assert!(
            snapshot
                .iter()
                .all(|node| node.value() != Some("private-value"))
        );
        page.update(cx, |page, cx| {
            page.password = true;
            cx.notify();
        });
        input.update(cx, |state, cx| state.set_masked(false, window, cx));
        let snapshot = tree(window, cx);
        assert_eq!(focused(&snapshot).role(), Some(gpui::Role::PasswordInput));
        assert_eq!(focused(&snapshot).value(), None);
        assert!(
            snapshot
                .iter()
                .all(|node| node.value() != Some("private-value"))
        );
    });
}

#[gpui::test]
fn accessibility_value_actions_respect_current_editability(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (page, cx) = cx.add_window_view(|window, cx| Inputs {
        input: cx.new(|cx| InputState::new(window, cx).default_value("initial")),
        textarea: cx.new(|cx| TextareaState::new(window, cx)),
        password: false,
        readonly: false,
        disabled: false,
    });
    cx.update(|window, cx| {
        let input = page.read(cx).input.clone();
        let replacement = gpui::accesskit::ActionData::Value("updated🦀".into());
        input.update(cx, |state, cx| {
            state.accessibility_set_value(Some(&replacement), window, cx);
        });
        assert_eq!(input.read(cx).value(), "updated🦀");
        for disabled in [false, true] {
            page.update(cx, |page, cx| {
                page.readonly = !disabled;
                page.disabled = disabled;
                cx.notify();
            });
            tree(window, cx);
            let rejected = gpui::accesskit::ActionData::Value("rejected".into());
            input.update(cx, |state, cx| {
                state.accessibility_set_value(Some(&rejected), window, cx);
            });
            assert_eq!(input.read(cx).value(), "updated🦀");
        }
    });
}
