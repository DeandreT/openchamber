//! One activation path for pointer and accessibility clients.
//!
//! GPUI answers an accessibility Click without a registered handler by
//! synthesizing a mouse click, which `on_click` ignores unless the pointer last
//! hovered the element. Controls therefore register the same workspace action
//! for both, so a screen reader or automation client activates exactly what a
//! pointer click does.

use std::rc::Rc;

use gpui_kit::{AccessibleAction, Context, StatefulInteractiveElement, Window};

use crate::workspace::Workspace;

type Action = dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

/// Runs `action` on a pointer click or an accessibility Click.
pub fn on_activate<E: StatefulInteractiveElement>(
    element: E,
    cx: &Context<Workspace>,
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> E {
    let action: Rc<Action> = Rc::new(action);
    on_accessible_click(on_pointer_click(element, cx, action.clone()), cx, action)
}

/// Runs `action` on a pointer click over `target` and an accessibility Click
/// on `node`: for a row whose accessible node keeps its list slot while its
/// pointer target spans the full sidebar width.
pub fn on_activate_split<N: StatefulInteractiveElement, T: StatefulInteractiveElement>(
    node: N,
    target: T,
    cx: &Context<Workspace>,
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> (N, T) {
    let action: Rc<Action> = Rc::new(action);
    (
        on_accessible_click(node, cx, action.clone()),
        on_pointer_click(target, cx, action),
    )
}

fn on_pointer_click<E: StatefulInteractiveElement>(
    element: E,
    cx: &Context<Workspace>,
    action: Rc<Action>,
) -> E {
    element.on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
}

fn on_accessible_click<E: StatefulInteractiveElement>(
    element: E,
    cx: &Context<Workspace>,
    action: Rc<Action>,
) -> E {
    let workspace = cx.entity().downgrade();
    element.on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
        let _ = workspace.update(cx, |this, cx| action(this, window, cx));
    })
}
