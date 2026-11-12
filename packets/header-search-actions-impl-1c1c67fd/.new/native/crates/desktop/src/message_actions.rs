//! The actions under a turn's last assistant message (`MessageBody.tsx`'s
//! footer): Copy the turn's answer, Continue (fork from here), Pin into
//! context and More (copy a link to the message, save the answer as a
//! plan). A prompt's row opens its own More menu here (pin into context,
//! fork from here); `branching_view.rs` draws that row.
//!
//! The row appears with the pointer over the message and stays while one of
//! its menus is open. Menus take the arrow keys, Ctrl+N/P, Home, End, Enter,
//! Space and Escape, skip a line that cannot run, and close on an outside
//! press. The pin button is drawn by `context_panel/pinned_section.rs`,
//! which owns the change; the plan dialog is `save_plan_dialog.rs`. Start
//! new session and the More menu's image, read-aloud and review-transfer
//! items are not ported.

use std::time::Duration;

use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectFirst, SelectLast, SelectUp};
use gpui_kit::{
    Anchor, AnyElement, ClipboardItem, Context, Edges, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled,
    TestSupportExt, Window, anchored, deferred, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::context_pins;
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
    /// The More menu of a prompt's row.
    Prompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuItem {
    ForkFromHere,
    CopyLink,
    SaveAsPlan,
    PinContext,
    UnpinContext,
    /// Fork with the prompt back in the composer.
    ForkFromPrompt,
}

impl MenuItem {
    fn label_key(self) -> &'static str {
        match self {
            Self::ForkFromHere | Self::ForkFromPrompt => "chat.messageBody.actions.fork",
            Self::CopyLink => "chat.messageBody.actions.copyLink",
            Self::SaveAsPlan => "chat.messageBody.actions.saveAsPlan",
            Self::PinContext => "chat.messageBody.actions.pinContext",
            Self::UnpinContext => "chat.messageBody.actions.unpinContext",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::ForkFromHere | Self::ForkFromPrompt => "git-branch",
            Self::CopyLink => "link",
            Self::SaveAsPlan => "booklet",
            Self::PinContext => "pushpin-2",
            Self::UnpinContext => "pushpin-2-fill",
        }
    }
}

/// One line of an open menu; a disabled line is drawn dimmed and runs
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MenuLine {
    pub(crate) item: MenuItem,
    pub(crate) enabled: bool,
}

/// What a message's menu lists, decided when it opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MenuFacts {
    /// The answer or the prompt has text.
    pub(crate) has_text: bool,
    /// Save as plan has a server and a knowledge owner to save to.
    pub(crate) can_save_plan: bool,
    /// The message has a creation time, which places a pin.
    pub(crate) pinnable: bool,
    pub(crate) pinned: bool,
    /// A pin change of the message is in flight.
    pub(crate) pin_pending: bool,
    /// A server can take a fork.
    pub(crate) available: bool,
}

/// The lines of a menu in the reference's order (`footerMenuActions` and
/// `userMenuActions`). Save as plan is always listed and waits for text and
/// an owner; a prompt's pin is listed only for a prompt with text.
pub(crate) fn menu_lines(kind: MenuKind, facts: MenuFacts) -> Vec<MenuLine> {
    let line = |item, enabled| MenuLine { item, enabled };
    match kind {
        MenuKind::Continue => vec![line(MenuItem::ForkFromHere, true)],
        MenuKind::More => vec![
            line(MenuItem::CopyLink, true),
            line(MenuItem::SaveAsPlan, facts.has_text && facts.can_save_plan),
        ],
        MenuKind::Prompt => {
            let mut lines = Vec::new();
            if facts.has_text && facts.pinnable {
                lines.push(line(
                    if facts.pinned {
                        MenuItem::UnpinContext
                    } else {
                        MenuItem::PinContext
                    },
                    !facts.pin_pending,
                ));
            }
            if facts.available {
                lines.push(line(MenuItem::ForkFromPrompt, true));
            }
            lines
        }
    }
}

/// An open footer or prompt menu.
pub(crate) struct MessageMenu {
    session: SessionId,
    message: MessageId,
    kind: MenuKind,
    pub(crate) lines: Vec<MenuLine>,
    highlighted: usize,
    focus: FocusHandle,
}

/// The footer's state: an open menu, the message whose text was just
/// copied and the plan the Save as plan dialog holds.
#[derive(Default)]
pub(crate) struct MessageActions {
    menu: Option<MessageMenu>,
    copied: Option<MessageId>,
    pub(crate) plan: Option<crate::save_plan_dialog::SavePlan>,
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
            .filter(|menu| menu.message == id && menu.kind != MenuKind::Prompt);
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

    pub(crate) fn render_message_menu(
        &self,
        menu: &MessageMenu,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let hover = palette.hover;
        let items = menu.lines.iter().enumerate().map(|(index, line)| {
            let MenuLine { item, enabled } = *line;
            let row = div()
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
                .map(|row| {
                    if enabled {
                        row.cursor_pointer()
                    } else {
                        row.opacity(0.5)
                            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    }
                })
                .when(enabled && menu.highlighted == index, |this| this.bg(hover))
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if enabled
                        && let Some(menu) = this.message_actions.menu.as_mut()
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
                );
            if enabled {
                on_activate(row, cx, move |this, window, cx| {
                    this.activate_message_menu_item(index, window, cx)
                })
                .test_support()
                .into_any_element()
            } else {
                row.test_support().into_any_element()
            }
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
        self.mark_message_copied(message, cx);
    }

    /// Copies a prompt's text from its row.
    pub(crate) fn copy_prompt_text(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self.message_text(session, message) else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.mark_message_copied(message, cx);
    }

    /// The text of a message the transcript of `session` holds.
    fn message_text(&self, session: &SessionId, message: &MessageId) -> Option<String> {
        let transcript = &self.open_sessions.get(session)?.transcript;
        Some(
            transcript
                .message(transcript.row_of(message)?)?
                .plain_text(),
        )
    }

    /// Whether the copy button of `message` shows its check.
    pub(crate) fn message_copied(&self, message: &MessageId) -> bool {
        self.message_actions.copied.as_ref() == Some(message)
    }

    /// The open More menu of the prompt `message`.
    pub(crate) fn prompt_menu(&self, message: &MessageId) -> Option<&MessageMenu> {
        self.message_actions
            .menu
            .as_ref()
            .filter(|menu| menu.kind == MenuKind::Prompt && menu.message == *message)
    }

    /// What the menu of `message` depends on now.
    pub(crate) fn message_menu_facts(
        &self,
        session: &SessionId,
        message: &MessageId,
        kind: MenuKind,
        cx: &gpui_kit::App,
    ) -> MenuFacts {
        let Some(open) = self.open_sessions.get(session) else {
            return MenuFacts::default();
        };
        let held = open
            .transcript
            .row_of(message)
            .and_then(|row| Some((row, open.transcript.message(row)?)));
        let Some((row, held)) = held else {
            return MenuFacts::default();
        };
        let text = if kind == MenuKind::Prompt {
            held.plain_text()
        } else {
            open.transcript.turn_answer_text(row)
        };
        MenuFacts {
            has_text: !text.trim().is_empty(),
            can_save_plan: kind == MenuKind::More && self.can_save_plan(session, cx),
            pinnable: held.info.created != 0,
            pinned: self
                .session(session)
                .is_some_and(|record| context_pins::is_pinned(record, message)),
            pin_pending: self.context_pin_pending(session, message),
            available: self.session_actions_available(),
        }
    }

    /// Shows the check on the copy button of `message` for two seconds.
    fn mark_message_copied(&mut self, message: &MessageId, cx: &mut Context<Self>) {
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

    pub(crate) fn toggle_message_menu(
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
        let lines = menu_lines(kind, self.message_menu_facts(session, message, kind, cx));
        if lines.is_empty() {
            self.close_message_menu(cx);
            return;
        }
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.message_actions.menu = Some(MessageMenu {
            session: session.clone(),
            message: message.clone(),
            kind,
            highlighted: lines.iter().position(|line| line.enabled).unwrap_or(0),
            lines,
            focus,
        });
        cx.notify();
    }

    /// Moves the highlight to the next line that can run; a step past
    /// either end stays, and a step of more than one line goes to that end.
    fn step_message_menu(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(menu) = self.message_actions.menu.as_mut() else {
            return;
        };
        let enabled = |index: &usize| menu.lines[*index].enabled;
        let count = menu.lines.len();
        let next = match delta {
            ..=-2 => (0..count).find(enabled),
            -1 => (0..menu.highlighted.min(count)).rev().find(enabled),
            2.. => (0..count).rev().find(enabled),
            _ => (menu.highlighted + 1..count).find(enabled),
        };
        if let Some(next) = next {
            menu.highlighted = next;
            cx.notify();
        }
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
        let Some(item) = menu
            .lines
            .get(index)
            .filter(|line| line.enabled)
            .map(|line| line.item)
        else {
            return;
        };
        match item {
            MenuItem::ForkFromHere => self.fork_after_message(menu.message, window, cx),
            MenuItem::CopyLink => {
                if let Some(link) = message_link(&menu.session, &menu.message) {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                }
            }
            MenuItem::SaveAsPlan => {
                let text = self
                    .open_sessions
                    .get(&menu.session)
                    .and_then(|open| {
                        let row = open.transcript.row_of(&menu.message)?;
                        Some(open.transcript.turn_answer_text(row))
                    })
                    .unwrap_or_default();
                self.open_save_plan_dialog(&menu.session, text, window, cx);
            }
            MenuItem::PinContext | MenuItem::UnpinContext => {
                self.toggle_context_pin(&menu.session, &menu.message, window, cx)
            }
            MenuItem::ForkFromPrompt => self.fork_from_message(menu.message, window, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(kind: MenuKind, facts: MenuFacts) -> Vec<(MenuItem, bool)> {
        menu_lines(kind, facts)
            .into_iter()
            .map(|line| (line.item, line.enabled))
            .collect()
    }

    #[test]
    fn the_more_menu_lists_the_link_and_the_plan_in_reference_order() {
        let ready = MenuFacts {
            has_text: true,
            can_save_plan: true,
            ..MenuFacts::default()
        };
        assert_eq!(
            items(MenuKind::More, ready),
            [(MenuItem::CopyLink, true), (MenuItem::SaveAsPlan, true)]
        );
        for facts in [
            MenuFacts {
                has_text: false,
                ..ready
            },
            MenuFacts {
                can_save_plan: false,
                ..ready
            },
        ] {
            assert_eq!(
                items(MenuKind::More, facts),
                [(MenuItem::CopyLink, true), (MenuItem::SaveAsPlan, false)],
                "the plan waits for text and an owner: {facts:?}"
            );
        }
        assert_eq!(
            items(MenuKind::Continue, MenuFacts::default()),
            [(MenuItem::ForkFromHere, true)]
        );
    }

    #[test]
    fn a_prompts_menu_pins_then_forks() {
        let ready = MenuFacts {
            has_text: true,
            pinnable: true,
            available: true,
            ..MenuFacts::default()
        };
        assert_eq!(
            items(MenuKind::Prompt, ready),
            [
                (MenuItem::PinContext, true),
                (MenuItem::ForkFromPrompt, true)
            ]
        );
        assert_eq!(
            items(
                MenuKind::Prompt,
                MenuFacts {
                    pinned: true,
                    pin_pending: true,
                    ..ready
                }
            ),
            [
                (MenuItem::UnpinContext, false),
                (MenuItem::ForkFromPrompt, true)
            ],
            "a pin change in flight dims its line"
        );
        for facts in [
            MenuFacts {
                has_text: false,
                ..ready
            },
            MenuFacts {
                pinnable: false,
                ..ready
            },
        ] {
            assert_eq!(
                items(MenuKind::Prompt, facts),
                [(MenuItem::ForkFromPrompt, true)],
                "{facts:?}"
            );
        }
        assert_eq!(
            items(
                MenuKind::Prompt,
                MenuFacts {
                    available: false,
                    ..ready
                }
            ),
            [(MenuItem::PinContext, true)],
            "no server, no fork"
        );
        assert!(items(MenuKind::Prompt, MenuFacts::default()).is_empty());
    }

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
