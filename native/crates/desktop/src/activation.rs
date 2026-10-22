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

/// Runs `action` on a pointer click or an accessibility Click.
pub fn on_activate<E: StatefulInteractiveElement>(
    element: E,
    cx: &Context<Workspace>,
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> E {
    // Both handlers outlive this call and run the same closure.
    let action = Rc::new(action);
    let pointer_action = action.clone();
    let workspace = cx.entity().downgrade();
    element
        .on_click(cx.listener(move |this, _, window, cx| pointer_action(this, window, cx)))
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
            let _ = workspace.update(cx, |this, cx| action(this, window, cx));
        })
}
