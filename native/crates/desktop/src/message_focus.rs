//! A request to show one message of a session in its transcript
//! (`packages/ui/src/lib/router/messageFocus.ts` and the link target of
//! `useChatTimelineScroll.ts`).
//!
//! Only the newest request stands. It stays until its message is on screen,
//! proves missing, another session or none is entered or it is a minute old. A
//! message the transcript holds is revealed at once. One that lies before the
//! loaded history is first ruled out with a single whole-message read, then
//! reached through [`Workspace::load_earlier_prompts`], the transcript's own
//! older-history loader, one page after another. This module starts no
//! history read of its own; it only asks that loader for the next page and
//! reads the state it leaves.

use std::time::{Duration, Instant};

use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::transcript::Message;
use openchamber_native_transport::error::TransportError;

use crate::locale;
use crate::workspace::{EarlierHistory, History, Workspace};

/// `MESSAGE_FOCUS_TTL_MS`.
pub(crate) const TTL: Duration = Duration::from_secs(60);

/// `isLinkIdentifier`: 1 to 128 ASCII letters, digits, `_` or `-`.
fn is_link_identifier(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// One standing request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusRequest {
    pub(crate) session: SessionId,
    pub(crate) message: MessageId,
    /// Tells a repeated press from the request it repeats.
    pub(crate) serial: u64,
    requested_at: Instant,
}

/// How far the search for a message outside the loaded history went.
enum Search {
    /// Nothing ran for the request yet.
    Fresh,
    /// The one read that rules out a message that does not exist.
    Checking { _read: Task<()> },
    /// Older pages are read until the message arrives. `awaiting_page` is
    /// set once a page read was seen running for this request, so a failure
    /// the loader kept from before the request does not end it.
    Paging { awaiting_page: bool },
}

struct Standing {
    request: FocusRequest,
    search: Search,
}

/// The newest message-focus request and its search.
#[derive(Default)]
pub(crate) struct MessageFocus {
    standing: Option<Standing>,
    next_serial: u64,
    /// Redraws the requesting control when the request expires.
    expiry: Option<Task<()>>,
}

impl MessageFocus {
    /// Replaces the standing request. `None` when an ID is not link-safe.
    pub(crate) fn request(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        now: Instant,
    ) -> Option<u64> {
        if !is_link_identifier(session.as_str()) || !is_link_identifier(message.as_str()) {
            return None;
        }
        self.next_serial += 1;
        let serial = self.next_serial;
        self.standing = Some(Standing {
            request: FocusRequest {
                session: session.clone(),
                message: message.clone(),
                serial,
                requested_at: now,
            },
            search: Search::Fresh,
        });
        Some(serial)
    }

    /// The request that still stands at `now`.
    pub(crate) fn standing(&self, now: Instant) -> Option<&FocusRequest> {
        self.standing
            .as_ref()
            .map(|standing| &standing.request)
            .filter(|request| now.saturating_duration_since(request.requested_at) <= TTL)
    }

    /// The message a request in `session` is still bringing to the screen.
    pub(crate) fn in_flight(&self, session: &SessionId, now: Instant) -> Option<&MessageId> {
        self.standing(now)
            .filter(|request| request.session == *session)
            .map(|request| &request.message)
    }

    /// Ends request `serial`; answers whether it was the standing one.
    pub(crate) fn settle(&mut self, serial: u64) -> bool {
        if self
            .standing
            .as_ref()
            .is_none_or(|standing| standing.request.serial != serial)
        {
            return false;
        }
        self.standing = None;
        self.expiry = None;
        true
    }

    /// Entering `session` drops a request that names another one.
    pub(crate) fn release_outside(&mut self, session: &SessionId) {
        if self
            .standing
            .as_ref()
            .is_some_and(|standing| standing.request.session != *session)
        {
            self.standing = None;
            self.expiry = None;
        }
    }

    /// Drops the request: no session is entered any more
    /// (`releaseMessageFocusOutside(null)`), or the runtime changed. Serials
    /// keep counting, so a completion of the dropped request never matches a
    /// later one.
    pub(crate) fn clear(&mut self) {
        self.standing = None;
        self.expiry = None;
    }

    fn search_mut(&mut self, serial: u64) -> Option<&mut Search> {
        self.standing
            .as_mut()
            .filter(|standing| standing.request.serial == serial)
            .map(|standing| &mut standing.search)
    }
}

impl Workspace {
    /// Asks the transcript of `session` to show `message`. Every call is a
    /// new request, also for the message the standing one names.
    pub(crate) fn request_message_focus(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(serial) = self.message_focus.request(session, message, Instant::now()) else {
            return;
        };
        self.message_focus.expiry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(TTL + Duration::from_millis(1))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.message_focus.settle(serial) {
                    cx.notify();
                }
            });
        }));
        self.advance_message_focus(window, cx);
        cx.notify();
    }

    /// Takes the standing request one step further: reveals its message when
    /// the transcript holds it, otherwise continues the search. Runs when the
    /// request is made and before every frame, so each page the history
    /// loader lands is looked at once.
    pub(crate) fn advance_message_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.message_focus.standing(Instant::now()).cloned() else {
            return;
        };
        if self.selected.as_ref() != Some(&request.session) {
            return;
        }
        let Some(open) = self.open_sessions.get(&request.session) else {
            return;
        };
        if let Some(row) = open.transcript.row_of(&request.message) {
            open.list.scroll_to_reveal_item(row);
            self.message_focus.settle(request.serial);
            cx.notify();
            return;
        }
        let history_loading = matches!(open.history, History::Loading { .. });
        let history_failed = matches!(open.history, History::Failed(_));
        let earlier_loading = matches!(open.earlier, EarlierHistory::Loading { .. });
        let earlier_failed = matches!(open.earlier, EarlierHistory::Failed(_));
        let has_older = open.older.is_some();
        // A connection without a client is reconnecting. Nothing can be asked
        // and nothing is known about the message, so the request stands until
        // the client is back or it expires.
        if self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.client.is_none())
        {
            return;
        }
        if history_loading {
            return;
        }
        if history_failed {
            return self.report_missing_focus(request.serial, window, cx);
        }
        let Some(search) = self.message_focus.search_mut(request.serial) else {
            return;
        };
        if matches!(search, Search::Fresh) {
            *search = Search::Paging {
                awaiting_page: false,
            };
            if self.start_focus_check(&request, window, cx) {
                return;
            }
        }
        let Some(Search::Paging { awaiting_page }) = self.message_focus.search_mut(request.serial)
        else {
            // The existence read is still running.
            return;
        };
        if earlier_loading {
            *awaiting_page = true;
            return;
        }
        if (earlier_failed && *awaiting_page) || !has_older {
            return self.report_missing_focus(request.serial, window, cx);
        }
        self.load_earlier_prompts(cx);
        let started = self
            .open_sessions
            .get(&request.session)
            .is_some_and(|open| matches!(open.earlier, EarlierHistory::Loading { .. }));
        if !started {
            return self.report_missing_focus(request.serial, window, cx);
        }
        if let Some(Search::Paging { awaiting_page }) =
            self.message_focus.search_mut(request.serial)
        {
            *awaiting_page = true;
        }
    }

    /// Starts the read that rules out a message that does not exist
    /// (`checkLinkedMessage`). Answers whether it started; synthetic data has
    /// no server to ask and the search goes ahead unchecked.
    fn start_focus_check(
        &mut self,
        request: &FocusRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self.connection.as_ref() else {
            return false;
        };
        let Some(client) = connection.client.clone() else {
            return false;
        };
        let (session, message) = (request.session.clone(), request.message.clone());
        let read = connection
            .network
            .run(async move { client.session_message(&session, &message).await });
        let serial = request.serial;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_focus_check(serial, result, window, cx);
            });
        });
        if let Some(search) = self.message_focus.search_mut(serial) {
            *search = Search::Checking { _read: task };
        }
        true
    }

    /// Only a 404 means the message is missing; any other failure is not an
    /// answer and the search goes ahead.
    pub(crate) fn finish_focus_check(
        &mut self,
        serial: u64,
        result: Result<Message, TransportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.message_focus.search_mut(serial) else {
            return;
        };
        if !matches!(search, Search::Checking { .. }) {
            return;
        }
        if matches!(result, Err(TransportError::Http { status: 404 })) {
            return self.report_missing_focus(serial, window, cx);
        }
        *search = Search::Paging {
            awaiting_page: false,
        };
        self.advance_message_focus(window, cx);
        cx.notify();
    }

    /// Ends the request and says the message was not found
    /// (`handleLinkedMessageMissing`).
    fn report_missing_focus(&mut self, serial: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.message_focus.settle(serial) {
            return;
        }
        let text = locale::strings(cx).get("chat.messageLink.notFound");
        self.push_notice(Notification::info(text.clone()), text, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_request_stands_and_a_repeat_is_a_new_one() {
        let mut focus = MessageFocus::default();
        let now = Instant::now();
        let (session, message) = (SessionId::from("ses_1"), MessageId::from("msg_1"));
        let first = focus.request(&session, &message, now).expect("link-safe");
        let second = focus.request(&session, &message, now).expect("link-safe");
        assert_ne!(first, second);
        assert_eq!(focus.in_flight(&session, now), Some(&message));
        assert!(!focus.settle(first), "a replaced request settles nothing");
        assert_eq!(focus.in_flight(&session, now), Some(&message));
        assert!(focus.settle(second));
        assert_eq!(focus.in_flight(&session, now), None);
    }

    #[test]
    fn a_request_leaves_with_another_session_or_after_a_minute() {
        let mut focus = MessageFocus::default();
        let now = Instant::now();
        let (session, message) = (SessionId::from("ses_1"), MessageId::from("msg_1"));
        focus.request(&session, &message, now);
        assert_eq!(focus.in_flight(&SessionId::from("ses_2"), now), None);
        focus.release_outside(&session);
        assert_eq!(focus.in_flight(&session, now + TTL), Some(&message));
        assert_eq!(
            focus.in_flight(&session, now + TTL + Duration::from_millis(1)),
            None
        );
        focus.request(&session, &message, now);
        focus.release_outside(&SessionId::from("ses_2"));
        assert_eq!(focus.in_flight(&session, now), None);
    }

    #[test]
    fn identifiers_that_are_not_link_safe_raise_no_request() {
        let mut focus = MessageFocus::default();
        let now = Instant::now();
        let session = SessionId::from("ses_1");
        for message in ["", "msg 1", "msg/1", &"m".repeat(129)] {
            assert_eq!(
                focus.request(&session, &MessageId::from(message), now),
                None
            );
        }
        assert_eq!(
            focus.request(&SessionId::from("ses:1"), &MessageId::from("msg_1"), now),
            None
        );
        assert!(focus.standing(now).is_none());
    }
}
