use gpui::accesskit::{Live, Role};
use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, VisualTestContext, Window, div,
};
use gpui_component::ActiveTheme as _;
use gpui_component::notification::{Notification, NotificationList};

struct Toasts;

struct Host {
    list: Entity<NotificationList>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.list.clone())
    }
}

/// The label and live setting of every toast in the next tree update.
fn toasts(cx: &mut VisualTestContext) -> Vec<(Option<String>, Option<Live>)> {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window
            .accessibility_tree_for_test()
            .expect("active accessibility tree")
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::Alert)
            .map(|(_, node)| (node.label().map(str::to_owned), node.live()))
            .collect()
    })
}

fn open(cx: &mut TestAppContext) -> (Entity<NotificationList>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|window, cx| Host {
        list: cx.new(|cx| NotificationList::new(window, cx)),
    });
    cx.update(|window, _| window.activate_accessibility_for_test());
    let list = host.read_with(cx, |host, _| host.list.clone());
    (list, cx)
}

fn push(
    list: &Entity<NotificationList>,
    notification: Notification,
    cx: &mut VisualTestContext,
) -> Vec<(Option<String>, Option<Live>)> {
    list.update_in(cx, |list, window, cx| {
        list.push(notification.id::<Toasts>().autohide(false), window, cx)
    });
    toasts(cx)
}

#[gpui::test]
fn a_shown_toast_is_a_named_polite_live_region(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    let shown = push(
        &list,
        Notification::error("HTTP 500").title("Couldn't unpin the session"),
        cx,
    );
    assert_eq!(
        shown,
        vec![(
            Some("Couldn't unpin the session\nHTTP 500".to_owned()),
            Some(Live::Polite)
        )],
        "the AT-SPI adapter announces a named live node when it appears"
    );
}

#[gpui::test]
fn a_toast_without_a_title_reads_its_message(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    let shown = push(&list, Notification::success("Session archived"), cx);
    assert_eq!(
        shown,
        vec![(Some("Session archived".to_owned()), Some(Live::Polite))]
    );
}

#[gpui::test]
fn a_closing_toast_is_not_announced_again(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    push(&list, Notification::info("Copied"), cx);
    list.update_in(cx, |list, window, cx| list.clear(window, cx));
    assert_eq!(
        toasts(cx),
        vec![(Some("Copied".to_owned()), None)],
        "the toast keeps its name while it fades out but leaves the live region"
    );
}

#[gpui::test]
fn a_toast_returning_to_a_full_stack_is_not_announced_again(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    let max_items = cx.update(|_, cx| cx.theme().notification.max_items);
    for index in 0..=max_items {
        list.update_in(cx, |list, window, cx| {
            let toast = Notification::info(format!("Toast {index}"))
                .id1::<Toasts>(index)
                .autohide(false);
            list.push(toast, window, cx)
        });
        toasts(cx);
    }
    // Past the enter transition: every toast is present.
    cx.background_executor
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let shown = toasts(cx);
    assert!(
        !shown
            .iter()
            .any(|(label, _)| label.as_deref() == Some("Toast 0")),
        "the oldest toast is pushed out of the full stack: {shown:?}"
    );

    let newest = list.read_with(cx, |list, _| {
        list.notifications()
            .last()
            .cloned()
            .expect("toasts are listed")
    });
    newest.update_in(cx, |toast, window, cx| toast.dismiss(window, cx));
    let returned = toasts(cx);
    assert_eq!(
        returned
            .iter()
            .find(|(label, _)| label.as_deref() == Some("Toast 0")),
        Some(&(Some("Toast 0".to_owned()), None)),
        "the returning toast's new node carries no live setting: {returned:?}"
    );
}

#[gpui::test]
fn a_toast_returning_during_its_enter_transition_is_not_announced_again(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    let max_items = cx.update(|_, cx| cx.theme().notification.max_items);
    for index in 0..=max_items {
        list.update_in(cx, |list, window, cx| {
            list.push(
                Notification::info(format!("Quick toast {index}"))
                    .id1::<Toasts>(index)
                    .autohide(false),
                window,
                cx,
            );
        });
        toasts(cx);
    }
    let newest = list.read_with(cx, |list, _| {
        list.notifications()
            .last()
            .cloned()
            .expect("the full stack has a newest toast")
    });
    newest.update_in(cx, |toast, window, cx| toast.dismiss(window, cx));
    let returned = toasts(cx);
    assert_eq!(
        returned
            .iter()
            .find(|(label, _)| label.as_deref() == Some("Quick toast 0")),
        Some(&(Some("Quick toast 0".to_owned()), None)),
        "a returning toast must stay silent even before its enter timer finishes"
    );
}

#[gpui::test]
fn an_unseen_toast_can_announce_when_it_first_enters_the_visible_stack(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    let max_items = cx.update(|_, cx| cx.theme().notification.max_items);
    list.update_in(cx, |list, window, cx| {
        for index in 0..=max_items {
            list.push(
                Notification::info(format!("Unseen toast {index}"))
                    .id1::<Toasts>(index)
                    .autohide(false),
                window,
                cx,
            );
        }
    });
    assert!(
        !toasts(cx)
            .iter()
            .any(|(label, _)| label.as_deref() == Some("Unseen toast 0")),
        "the oldest toast has not appeared in the accessibility tree yet"
    );
    cx.background_executor
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let newest = list.read_with(cx, |list, _| {
        list.notifications()
            .last()
            .cloned()
            .expect("the stack has a newest toast")
    });
    newest.update_in(cx, |toast, window, cx| toast.dismiss(window, cx));
    assert_eq!(
        toasts(cx)
            .iter()
            .find(|(label, _)| label.as_deref() == Some("Unseen toast 0")),
        Some(&(Some("Unseen toast 0".to_owned()), Some(Live::Polite))),
        "a toast hidden before its first accessible frame still gets one announcement"
    );
}

#[gpui::test]
fn replacing_a_toast_announces_the_new_message(cx: &mut TestAppContext) {
    let (list, cx) = open(cx);
    push(&list, Notification::info("First message"), cx);
    assert_eq!(
        push(&list, Notification::info("Replacement message"), cx),
        vec![(Some("Replacement message".to_owned()), Some(Live::Polite))],
        "replacing an existing notification creates a new announcement"
    );
}
