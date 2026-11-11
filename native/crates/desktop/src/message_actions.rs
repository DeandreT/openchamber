//! The actions under a turn's last assistant message (`MessageBody.tsx`'s
//! footer): Copy the turn's answer, Continue (fork from here), Pin into
//! context and More (copy a link to the message).
//!
//! The row appears with the pointer over the message and stays while one of
//! its menus is open. Menus take the arrow keys, Ctrl+N/P, Home, End, Enter,
//! Space and Escape, and close on an outside press. The pin button is drawn
//! by `context_panel/pinned_section.rs`, which owns the change. Start new
//! session and the More menu's export, read-aloud and plan items are not
//! ported.

use std::time::Duration;

use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectFirst, SelectLast, SelectUp};
use gpui_kit::{
    Anchor, AnyElement, ClipboardItem, Context, Edges, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled,
    TestSupportExt, Window, anchored, deferred, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::transcript::Message;

use crate::activation::on_activate;
use crate::chrome::{IconButton, glyph};
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;
use crate::workspace::Workspace;

/// Copy, continue, pin and more: four 26 px buttons with `gap-1.5`.
pub(crate) const ACTIONS_WIDTH: f32 = 4.0 * 26.0 + 3.0 * 6.0;
/// The group the transcript gives every message row, for hover reveal.
pub(crate) const MESSAGE_GROUP: &str = crate::branching_view::MESSAGE_GROUP;
const COPIED_FOR: Duration = Duration::from_secs(2);
const MENU_ID: &str = "message-actions-menu";
/// A deep link to one message (`buildMessageLink`, desktop form).
const DEEP_LINK_PREFIX: &str = "openchamber://session/";

/// Which footer menu is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuKind {
    Continue,
    More,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuItem {
    ForkFromHere,
    CopyLink,
}

impl MenuItem {
    fn label_key(self) -> &'static str {
        match self {
            Self::ForkFromHere => "chat.messageBody.actions.fork",
            Self::CopyLink => "chat.messageBody.actions.copyLink",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::ForkFromHere => "git-branch",
            Self::CopyLink => "link",
        }
    }
}

/// An open footer menu.
pub(crate) struct MessageMenu {
    session: SessionId,
    message: MessageId,
    kind: MenuKind,
    highlighted: usize,
    focus: FocusHandle,
}

impl MessageMenu {
    fn items(&self) -> &'static [MenuItem] {
        match self.kind {
            MenuKind::Continue => &[MenuItem::ForkFromHere],
            MenuKind::More => &[MenuItem::CopyLink],
        }
    }
}

/// The footer's state: an open menu and the message whose answer was just
/// copied.
#[derive(Default)]
pub(crate) struct MessageActions {
    menu: Option<MessageMenu>,
    copied: Option<MessageId>,
}

/// `openchamber://session/<session>?message=<message>`, when both IDs are
/// link-safe (`isLinkIdentifier`).
pub(crate) fn message_link(session: &SessionId, message: &MessageId) -> Option<String> {
    let safe = |id: &str| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    (safe(session.as_str()) && safe(message.as_str())).then(|| {
        format!(
            "{DEEP_LINK_PREFIX}{}?message={}",
            session.as_str(),
            message.as_str()
        )
    })
}

impl Workspace {
    /// The footer's action buttons, in the space the facts leave them.
    pub(crate) fn render_message_actions(
        &self,
        row: usize,
        message: &Message,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(session) = self.selected.clone() else {
            return div().w(px(ACTIONS_WIDTH)).into_any_element();
        };
        let strings = locale::strings(cx);
        let id = message.info.id.clone();
        let copyable = self
            .open_sessions
            .get(&session)
            .is_some_and(|open| !open.transcript.turn_answer_text(row).trim().is_empty());
        let copied = self.message_actions.copied.as_ref() == Some(&id);
        let menu_open = self
            .message_actions
            .menu
            .as_ref()
            .filter(|menu| menu.message == id);
        let mut buttons = div()
            .flex()
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0));
        if copyable {
            let (copy_session, copy_message) = (session.clone(), id.clone());
            buttons = buttons.child(
                IconButton::new(
                    format!("message-copy:{id}"),
                    if copied { "check" } else { "file-copy" },
                    strings.get("chat.messageBody.actions.copyMessageAria"),
                )
                .size(26.0, 12.0)
                .render(palette, cx, move |this, _, cx| {
                    this.copy_turn_answer(&copy_session, &copy_message, row, cx)
                }),
            );
        }
        for kind in [MenuKind::Continue, MenuKind::More] {
            // The pin sits between the two menus and, as Copy does, needs an
            // answer with text.
            if kind == MenuKind::More && copyable {
                buttons = buttons.child(self.render_context_pin_button(&session, &id, palette, cx));
            }
            let (icon, label, name) = match kind {
                MenuKind::Continue => (
                    "chat-forward",
                    "chat.messageBody.actions.branchMenu",
                    "continue",
                ),
                MenuKind::More => ("more", "chat.messageBody.actions.moreActions", "more"),
            };
            let (menu_session, menu_message) = (session.clone(), id.clone());
            buttons = buttons.child(
                IconButton::new(format!("message-{name}:{id}"), icon, strings.get(label))
                    .size(26.0, 12.0)
                    .render(palette, cx, move |this, window, cx| {
                        this.toggle_message_menu(&menu_session, &menu_message, kind, window, cx)
                    }),
            );
        }
        div()
            .relative()
            .w(px(ACTIONS_WIDTH))
            .flex_shrink_0()
            .flex()
            .flex_row_from(direction)
            .justify_end()
            .child(
                div()
                    .when(menu_open.is_none(), |this| {
                        this.opacity(0.0)
                            .group_hover(MESSAGE_GROUP, |style| style.opacity(1.0))
                    })
                    .child(buttons),
            )
            .children(menu_open.map(|menu| self.render_message_menu(menu, palette, direction, cx)))
            .into_any_element()
    }

    fn render_message_menu(
        &self,
        menu: &MessageMenu,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let hover = palette.hover;
        let items = menu.items().iter().enumerate().map(|(index, item)| {
            let item = *item;
            on_activate(
                div()
                    .id(SharedString::from(format!("{MENU_ID}:{index}")))
                    .role(Role::MenuItem)
                    .accessibility_id(format!("{MENU_ID}:{index}"))
                    .aria_label(strings.get(item.label_key()))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .when(menu.highlighted == index, |this| this.bg(hover))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if let Some(menu) = this.message_actions.menu.as_mut()
                            && menu.highlighted != index
                        {
                            menu.highlighted = index;
                            cx.notify();
                        }
                    }))
                    .child(glyph(item.icon(), 14.0, palette.muted_foreground))
                    .child(
                        div()
                            .text_size(rems(META_SIZE))
                            .text_color(palette.foreground)
                            .child(strings.get(item.label_key())),
                    ),
                cx,
                move |this, window, cx| this.activate_message_menu_item(index, window, cx),
            )
            .test_support()
        });
        let popup = div()
            .id(MENU_ID)
            .role(Role::Menu)
            .accessibility_id(MENU_ID)
            .track_focus(&menu.focus)
            .key_context(crate::session_menu::MENU_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_message_menu(1, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_message_menu(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.step_message_menu(-100, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.step_message_menu(100, cx)))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                let index = this
                    .message_actions
                    .menu
                    .as_ref()
                    .map_or(0, |menu| menu.highlighted);
                this.activate_message_menu_item(index, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Cancel, _, cx| this.close_message_menu(cx)))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_message_menu(cx)))
            .occlude()
            .min_w(px(180.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .flex()
            .flex_col()
            .children(items)
            .test_support();
        let anchor = match direction {
            Direction::LeftToRight => Anchor::TopRight,
            Direction::RightToLeft => Anchor::TopLeft,
        };
        deferred(
            anchored()
                .anchor(anchor)
                .snap_to_window_with_margin(Edges::all(px(8.0)))
                .child(div().mt(px(30.0)).child(popup)),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn copy_turn_answer(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        row: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open_sessions.get(session) else {
            return;
        };
        let text = open.transcript.turn_answer_text(row);
        if text.trim().is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.message_actions.copied = Some(message.clone());
        let copied = message.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.message_actions.copied.as_ref() == Some(&copied) {
                    this.message_actions.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn toggle_message_menu(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        kind: MenuKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .message_actions
            .menu
            .as_ref()
            .is_some_and(|menu| &menu.message == message && menu.kind == kind)
        {
            self.close_message_menu(cx);
            return;
        }
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.message_actions.menu = Some(MessageMenu {
            session: session.clone(),
            message: message.clone(),
            kind,
            highlighted: 0,
            focus,
        });
        cx.notify();
    }

    fn step_message_menu(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(menu) = self.message_actions.menu.as_mut() else {
            return;
        };
        let last = menu.items().len().saturating_sub(1) as isize;
        menu.highlighted = (menu.highlighted as isize + delta).clamp(0, last) as usize;
        cx.notify();
    }

    fn close_message_menu(&mut self, cx: &mut Context<Self>) {
        if self.message_actions.menu.take().is_some() {
            cx.notify();
        }
    }

    fn activate_message_menu_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.message_actions.menu.take() else {
            return;
        };
        cx.notify();
        match menu.items().get(index) {
            Some(MenuItem::ForkFromHere) => self.fork_after_message(menu.message, window, cx),
            Some(MenuItem::CopyLink) => {
                if let Some(link) = message_link(&menu.session, &menu.message) {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_the_session_and_message() {
        assert_eq!(
            message_link(&SessionId::from("ses_1"), &MessageId::from("msg_2")).as_deref(),
            Some("openchamber://session/ses_1?message=msg_2")
        );
        assert_eq!(
            message_link(&SessionId::from("ses 1"), &MessageId::from("msg_2")),
            None
        );
    }
}
