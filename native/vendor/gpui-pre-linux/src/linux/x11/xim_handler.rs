use std::default::Default;

use x11rb::protocol::{Event, xproto};
use xim::{AHashMap, AttributeName, Client, ClientError, ClientHandler, InputStyle};

pub enum XimCallbackEvent {
    XimXEvent(x11rb::protocol::Event),
    XimPreeditEvent(xproto::Window, String),
    XimCommitEvent(xproto::Window, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(handler: &mut XimHandler, first: i32, length: i32, text: &str) {
        handler.draw_preedit(first, length, Some(text));
    }

    fn take_preedit(handler: &mut XimHandler) -> Option<String> {
        match handler.last_callback_event.take() {
            Some(XimCallbackEvent::XimPreeditEvent(_, text)) => Some(text),
            None => None,
            _ => panic!("expected a preedit callback"),
        }
    }

    #[test]
    fn incremental_preedit_uses_character_offsets() {
        let mut handler = XimHandler::new();
        draw(&mut handler, 0, 0, "你🙂好");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some("你🙂好"));
        draw(&mut handler, 1, 1, "界");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some("你界好"));
        draw(&mut handler, 2, 0, "e\u{301}");
        assert_eq!(
            take_preedit(&mut handler).as_deref(),
            Some("你界e\u{301}好")
        );
        draw(&mut handler, 2, 2, "");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some("你界好"));
    }

    #[test]
    fn done_cancels_preedit_without_a_final_draw() {
        let mut handler = XimHandler::new();
        draw(&mut handler, 0, 0, "你好");
        take_preedit(&mut handler);
        handler.end_preedit();
        assert_eq!(take_preedit(&mut handler).as_deref(), Some(""));
        handler.start_preedit();
        draw(&mut handler, 0, 0, "新");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some("新"));
    }

    #[test]
    fn invalid_ranges_do_not_emit_text_and_later_valid_edits_work() {
        let mut handler = XimHandler::new();
        draw(&mut handler, 0, 0, "你好");
        take_preedit(&mut handler);
        for (first, length) in [(-1, 0), (0, -1), (3, 0), (1, 2), (i32::MAX, i32::MAX)] {
            draw(&mut handler, first, length, "bad");
            assert!(
                take_preedit(&mut handler).is_none(),
                "invalid range emitted text"
            );
        }
        draw(&mut handler, 1, 1, "们");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some("你们"));
    }

    #[test]
    fn ibus_commit_cleanup_never_rewrites_the_committed_text() {
        for committed in ["nihao", "你好"] {
            let mut handler = XimHandler::new();
            handler.start_preedit();
            // Captured IBus Draw changes are complete replacements, in characters.
            for (length, text) in [(0, "年"), (1, "你"), (1, "你和"), (2, "你哈"), (2, "你好")]
            {
                draw(&mut handler, 0, length, text);
                assert_eq!(take_preedit(&mut handler).as_deref(), Some(text));
            }
            handler.commit(committed);
            match handler.last_callback_event.take() {
                Some(XimCallbackEvent::XimCommitEvent(_, text)) => assert_eq!(text, committed),
                _ => panic!("commit was not delivered"),
            }
            draw(&mut handler, 0, 2, "");
            assert!(take_preedit(&mut handler).is_none());
            handler.end_preedit();
            assert!(take_preedit(&mut handler).is_none());
            handler.start_preedit();
            draw(&mut handler, 0, 0, "新");
            assert_eq!(take_preedit(&mut handler).as_deref(), Some("新"));
        }
    }

    #[test]
    fn feedback_updates_and_empty_idle_callbacks_preserve_text() {
        let mut handler = XimHandler::new();
        draw(&mut handler, 0, 0, "");
        assert!(take_preedit(&mut handler).is_none());
        draw(&mut handler, 0, 0, "你好");
        take_preedit(&mut handler);
        handler.draw_preedit(0, 2, None);
        assert!(take_preedit(&mut handler).is_none());
        draw(&mut handler, 1, 0, "们");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some("你们好"));
        draw(&mut handler, 0, 3, "");
        assert_eq!(take_preedit(&mut handler).as_deref(), Some(""));
        handler.end_preedit();
        assert!(take_preedit(&mut handler).is_none());
    }

    #[test]
    fn pending_composition_remains_owned_by_its_original_window() {
        let mut handler = XimHandler::new();
        handler.window = 10;
        handler.start_preedit();
        draw(&mut handler, 0, 0, "你好");
        take_preedit(&mut handler);
        handler.window = 20;
        handler.end_preedit();
        assert!(
            matches!(handler.last_callback_event.take(), Some(XimCallbackEvent::XimPreeditEvent(10, text)) if text.is_empty())
        );
        handler.start_preedit();
        draw(&mut handler, 0, 0, "新");
        assert!(
            matches!(handler.last_callback_event.take(), Some(XimCallbackEvent::XimPreeditEvent(20, text)) if text == "新")
        );
    }

    #[test]
    fn done_before_commit_and_commit_without_cleanup_preserve_commits() {
        for done_first in [false, true] {
            let mut handler = XimHandler::new();
            draw(&mut handler, 0, 0, "你好");
            take_preedit(&mut handler);
            if done_first {
                handler.end_preedit();
                assert_eq!(take_preedit(&mut handler).as_deref(), Some(""));
            }
            handler.commit("你好");
            assert!(
                matches!(handler.last_callback_event.take(), Some(XimCallbackEvent::XimCommitEvent(_, text)) if text == "你好")
            );
            handler.end_preedit();
            assert!(take_preedit(&mut handler).is_none());
        }
    }
}

pub struct XimHandler {
    pub im_id: u16,
    pub ic_id: u16,
    pub connected: bool,
    pub window: xproto::Window,
    pub last_callback_event: Option<XimCallbackEvent>,
    preedit: String,
    preedit_window: Option<xproto::Window>,
    preedit_committed: bool,
}

impl XimHandler {
    pub fn new() -> Self {
        Self {
            im_id: Default::default(),
            ic_id: Default::default(),
            connected: false,
            window: Default::default(),
            last_callback_event: None,
            preedit: String::new(),
            preedit_window: None,
            preedit_committed: false,
        }
    }

    fn start_preedit(&mut self) {
        self.end_preedit();
        self.preedit_window = Some(self.window);
    }

    fn end_preedit(&mut self) {
        if !self.preedit.is_empty() && !self.preedit_committed {
            self.last_callback_event = Some(XimCallbackEvent::XimPreeditEvent(
                self.preedit_window.unwrap_or(self.window),
                String::new(),
            ));
        }
        self.preedit.clear();
        self.preedit_window = None;
        self.preedit_committed = false;
    }

    fn draw_preedit(&mut self, first: i32, length: i32, text: Option<&str>) {
        // XIM offsets count characters, while String replacement requires byte offsets.
        let range = (|| {
            let first = usize::try_from(first).ok()?;
            let end = first.checked_add(usize::try_from(length).ok()?)?;
            let mut boundaries = self
                .preedit
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(self.preedit.len()));
            let start_byte = boundaries.nth(first)?;
            let end_byte = if end == first {
                start_byte
            } else {
                boundaries.nth(end - first - 1)?
            };
            Some(start_byte..end_byte)
        })();
        let Some(range) = range else {
            // Reject only this callback. Returning InvalidReply would tear down XIM.
            log::warn!("Ignoring invalid XIM preedit character range");
            return;
        };
        let Some(text) = text else {
            // Feedback-only updates change highlighting, not the preedit text.
            return;
        };
        let was_empty = self.preedit.is_empty();
        self.preedit.replace_range(range, text);
        // IBus sends a deletion of the old preedit after Commit. It must not edit
        // the newly committed text or create a second empty composition.
        if self.preedit.is_empty() && (self.preedit_committed || was_empty) {
            return;
        }
        self.preedit_committed = false;
        let window = *self.preedit_window.get_or_insert(self.window);
        self.last_callback_event = Some(XimCallbackEvent::XimPreeditEvent(
            window,
            self.preedit.clone(),
        ));
    }

    fn commit(&mut self, text: &str) {
        // Retain the protocol buffer until its cleanup Draw/Done arrives.
        self.preedit_committed = true;
        self.last_callback_event = Some(XimCallbackEvent::XimCommitEvent(
            self.preedit_window.unwrap_or(self.window),
            text.to_owned(),
        ));
    }
}

impl<C: Client<XEvent = xproto::KeyPressEvent>> ClientHandler<C> for XimHandler {
    fn handle_connect(&mut self, client: &mut C) -> Result<(), ClientError> {
        client.open("C")
    }

    fn handle_open(&mut self, client: &mut C, input_method_id: u16) -> Result<(), ClientError> {
        self.im_id = input_method_id;

        client.get_im_values(input_method_id, &[AttributeName::QueryInputStyle])
    }

    fn handle_get_im_values(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        _attributes: AHashMap<AttributeName, Vec<u8>>,
    ) -> Result<(), ClientError> {
        let ic_attributes = client
            .build_ic_attributes()
            .push(AttributeName::InputStyle, InputStyle::PREEDIT_CALLBACKS)
            .push(AttributeName::ClientWindow, self.window)
            .push(AttributeName::FocusWindow, self.window)
            .build();
        client.create_ic(input_method_id, ic_attributes)
    }

    fn handle_create_ic(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.connected = true;
        self.ic_id = input_context_id;
        Ok(())
    }

    fn handle_commit(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        _input_context_id: u16,
        text: &str,
    ) -> Result<(), ClientError> {
        self.commit(text);
        Ok(())
    }

    fn handle_forward_event(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        _input_context_id: u16,
        _flag: xim::ForwardEventFlag,
        xev: C::XEvent,
    ) -> Result<(), ClientError> {
        match xev.response_type {
            x11rb::protocol::xproto::KEY_PRESS_EVENT => {
                self.last_callback_event = Some(XimCallbackEvent::XimXEvent(Event::KeyPress(xev)));
            }
            x11rb::protocol::xproto::KEY_RELEASE_EVENT => {
                self.last_callback_event =
                    Some(XimCallbackEvent::XimXEvent(Event::KeyRelease(xev)));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_close(&mut self, client: &mut C, _input_method_id: u16) -> Result<(), ClientError> {
        client.disconnect()
    }

    fn handle_preedit_draw(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        _input_context_id: u16,
        _caret: i32,
        chg_first: i32,
        chg_len: i32,
        status: xim::PreeditDrawStatus,
        preedit_string: &str,
        feedbacks: Vec<xim::Feedback>,
    ) -> Result<(), ClientError> {
        // XIMReverse: 1, XIMPrimary: 8, XIMTertiary: 32: selected text
        // XIMUnderline: 2, XIMSecondary: 16: underlined text
        // XIMHighlight: 4: normal text
        // XIMVisibleToForward: 64, XIMVisibleToBackward: 128, XIMVisibleCenter: 256: text align position
        // XIMPrimary, XIMHighlight, XIMSecondary, XIMTertiary are not specified,
        // but interchangeable as above
        // Currently there's no way to support these.
        let text = if !status.contains(xim::PreeditDrawStatus::NO_STRING)
            && preedit_string.is_empty()
            && !feedbacks.is_empty()
        {
            None
        } else {
            Some(preedit_string)
        };
        self.draw_preedit(chg_first, chg_len, text);
        Ok(())
    }

    fn handle_preedit_start(
        &mut self,
        _client: &mut C,
        _im: u16,
        _ic: u16,
    ) -> Result<(), ClientError> {
        self.start_preedit();
        Ok(())
    }

    fn handle_preedit_done(
        &mut self,
        _client: &mut C,
        _im: u16,
        _ic: u16,
    ) -> Result<(), ClientError> {
        self.end_preedit();
        Ok(())
    }

    fn handle_reset_ic(
        &mut self,
        _client: &mut C,
        _im: u16,
        _ic: u16,
        _text: &str,
    ) -> Result<(), ClientError> {
        // The platform requests reset to cancel composition on focus changes.
        self.end_preedit();
        Ok(())
    }
}
