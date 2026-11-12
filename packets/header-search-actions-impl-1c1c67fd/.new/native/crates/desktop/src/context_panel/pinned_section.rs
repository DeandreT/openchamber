//! The work status card's Pinned messages section
//! (`WorkStatusPinnedSection.tsx`) and the pin button under an answer
//! (`MessageBody.tsx`): the messages pinned into the selected session's
//! context, each a row that goes to its message and a pin that unpins it.
//!
//! The pins are the session record's
//! (`metadata.openchamber.context_obligatory_messages`); nothing here keeps
//! its own copy of them. A row reads its line from the transcript. A pin
//! whose message lies before the loaded history reads that one message from
//! the server, once: a read that fails or finds no text leaves the
//! placeholder until the section is shown again.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use gpui_kit::{
    AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, Task, TestSupportExt, Toggled, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::context_pins::{self, ContextPin, PinRole, PinTime};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::session::{MetadataFacts, SessionPatch};
use openchamber_native_core::transcript::{Message, Role as MessageRole, Transcript};
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;

use super::work_status_sections::Layout;
use super::{ROW_HEIGHT, ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::send_attempt::SessionKey;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The pin and the spinner (`size-3.5`).
const GLYPH: f32 = 14.0;
/// The footer's pin button and its glyph, as its neighbours.
const FOOTER_BUTTON: f32 = 26.0;
const FOOTER_GLYPH: f32 = 12.0;
/// A row shows one truncated line of a 324 px card, so a line keeps only its
/// first characters instead of a whole message.
const LINE_CHARS: usize = 512;

/// What the fetched lines belong to. They are dropped when any of it changes
/// or the section leaves the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Scope {
    runtime: RuntimeId,
    session: SessionId,
    directory: Option<String>,
}

/// Who asked for a change to a session's context pins. It decides the
/// failure's notice and which control the change holds meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// A row's pin in the section.
    Section,
    /// The pin button under an answer.
    Footer,
}

/// One change to a session's context pins, waiting or in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinChange {
    message: MessageId,
    /// The entry to store; `None` unpins.
    pin: Option<ContextPin>,
    origin: Origin,
}

impl PinChange {
    /// The section's unpin of `message`.
    pub(crate) fn unpin(message: MessageId) -> Self {
        Self {
            message,
            pin: None,
            origin: Origin::Section,
        }
    }

    /// The footer button's change: store `pin`, or unpin without one.
    pub(crate) fn toggle(message: MessageId, pin: Option<ContextPin>) -> Self {
        Self {
            message,
            pin,
            origin: Origin::Footer,
        }
    }
}

/// The section's state beside the session record: fetched lines and the pin
/// changes waiting or in flight.
#[derive(Default)]
pub(crate) struct PinnedMessages {
    scope: Option<Scope>,
    /// Counts scope changes, so a read started under a retired scope does
    /// not publish.
    epoch: u64,
    /// The outcome of each whole-message read of this scope; `None` when the
    /// read failed or the message has no text.
    fetched: HashMap<MessageId, Option<SharedString>>,
    reads: HashMap<MessageId, Task<()>>,
    /// Lines of loaded messages by the length of the text they came from.
    /// A message's text only grows while it streams.
    loaded: RefCell<HashMap<MessageId, (usize, Option<SharedString>)>>,
    /// The section's unpin from its press until its answer. One runs at a
    /// time; presses on other pins meanwhile do nothing.
    unpinning: Option<(SessionKey, MessageId)>,
    /// The footer buttons held from their press until their answer.
    toggling: HashSet<(SessionKey, MessageId)>,
    /// Changes per session in press order. Every change rewrites the whole
    /// stored list from a fresh read, so two of one session never overlap.
    change_queue: HashMap<SessionKey, VecDeque<PinChange>>,
    changing: HashSet<SessionKey>,
}

/// One pin as its row shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinnedRow {
    pub(crate) id: MessageId,
    /// `None` shows the placeholder.
    pub(crate) text: Option<SharedString>,
}

fn clip_line(text: String) -> SharedString {
    match text.char_indices().nth(LINE_CHARS) {
        Some((end, _)) => SharedString::from(text[..end].to_owned()),
        None => SharedString::from(text),
    }
}

impl PinnedMessages {
    /// Forgets everything for a runtime switch. Requests in flight finish
    /// against their captured scope and runtime.
    pub(crate) fn switch_runtime(&mut self) {
        self.remount(None);
        self.unpinning = None;
        self.toggling.clear();
        self.change_queue.clear();
        self.changing.clear();
    }

    /// The epoch reads of the current scope carry.
    #[cfg(test)]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The whole-message reads in flight.
    #[cfg(test)]
    pub(crate) fn reading(&self) -> Vec<MessageId> {
        let mut ids: Vec<MessageId> = self.reads.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Whether a whole-message read of `message` ended in this scope.
    #[cfg(test)]
    pub(crate) fn fetched(&self, message: &MessageId) -> bool {
        self.fetched.contains_key(message)
    }

    #[cfg(test)]
    pub(crate) fn unpinning(&self) -> Option<&MessageId> {
        self.unpinning.as_ref().map(|(_, message)| message)
    }

    #[cfg(test)]
    pub(crate) fn begin_unpin(&mut self, key: SessionKey, message: MessageId) {
        self.unpinning = Some((key, message));
    }

    fn remount(&mut self, scope: Option<Scope>) {
        self.scope = scope;
        self.epoch += 1;
        self.fetched.clear();
        self.reads.clear();
        self.loaded.borrow_mut().clear();
    }

    /// The line of `message` when the transcript holds it with text.
    fn loaded_text(&self, transcript: &Transcript, message: &MessageId) -> Option<SharedString> {
        let held = transcript.message(transcript.row_of(message)?)?;
        let text = context_pins::first_text_part(held)?;
        let mut loaded = self.loaded.borrow_mut();
        if let Some((length, line)) = loaded.get(message)
            && *length == text.len()
        {
            return line.clone();
        }
        let compact = context_pins::compact_text(text).map(clip_line);
        loaded.insert(message.clone(), (text.len(), compact.clone()));
        compact
    }
}

impl Workspace {
    /// The selected session's pins in conversation order, each with the line
    /// it shows: the loaded message's, else the fetched one.
    pub(crate) fn pinned_rows(&self) -> Vec<PinnedRow> {
        let Some(session) = self.selected_session() else {
            return Vec::new();
        };
        let open = self.open_sessions.get(&session.id);
        context_pins::owned(session)
            .into_iter()
            .map(|pin| PinnedRow {
                id: pin.id.clone(),
                text: open
                    .and_then(|open| self.context_pins.loaded_text(&open.transcript, &pin.id))
                    .or_else(|| self.context_pins.fetched.get(&pin.id).cloned().flatten()),
            })
            .collect()
    }

    fn pinned_scope(&self, cx: &App) -> Option<Scope> {
        let session = self.selected_session()?;
        Layout::for_workspace(self, cx)
            .visible_builtin_order()
            .any(|id| id == "pinned")
            .then(|| Scope {
                runtime: self.runtime.clone(),
                session: session.id.clone(),
                directory: session.resolved_directory(),
            })
    }

    fn connection_generation(&self) -> Option<u64> {
        self.connection
            .as_ref()
            .map(|connection| connection.generation)
    }

    /// Starts the whole-message reads the section needs. `card_shown` says
    /// whether the work status card is on screen; while it is not, or the
    /// section is hidden, the fetched lines are dropped, so showing the
    /// section again reads them anew. Without a client nothing is read and
    /// nothing is recorded, so the lines are read once the client is back.
    pub(crate) fn ensure_pinned_messages(&mut self, card_shown: bool, cx: &mut Context<Self>) {
        let scope = if card_shown {
            self.pinned_scope(cx)
        } else {
            None
        };
        if self.context_pins.scope != scope {
            self.context_pins.remount(scope);
        }
        if self.context_pins.scope.is_none() {
            return;
        }
        if self
            .connection
            .as_ref()
            .is_none_or(|connection| connection.client.is_none())
        {
            return;
        }
        let unresolved: Vec<MessageId> = self
            .pinned_rows()
            .into_iter()
            .filter(|row| {
                row.text.is_none()
                    && !self.context_pins.fetched.contains_key(&row.id)
                    && !self.context_pins.reads.contains_key(&row.id)
            })
            .map(|row| row.id)
            .collect();
        for message in unresolved {
            self.read_pinned_message(message, cx);
        }
    }

    /// Reads one pinned message whole.
    fn read_pinned_message(&mut self, message: MessageId, cx: &mut Context<Self>) {
        let Some(scope) = self.context_pins.scope.clone() else {
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let epoch = self.context_pins.epoch;
        let generation = Some(connection.generation);
        let (request_session, request_message) = (scope.session, message.clone());
        let read = connection.network.run(async move {
            client
                .session_message(&request_session, &request_message)
                .await
        });
        let finished = message.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = read.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update(cx, |this, cx| {
                this.finish_pinned_read(epoch, generation, finished, result, cx);
            });
        });
        self.context_pins.reads.insert(message, task);
    }

    /// Publishes a whole-message read into the scope it started under. A
    /// failure is remembered as no text, so it is not read again. The outcome
    /// of a read whose connection was restarted meanwhile is dropped
    /// unrecorded: it says nothing about the message, and the next frame with
    /// a client reads it again.
    pub(crate) fn finish_pinned_read(
        &mut self,
        epoch: u64,
        generation: Option<u64>,
        message: MessageId,
        result: Result<Message, TransportError>,
        cx: &mut Context<Self>,
    ) {
        if self.context_pins.epoch != epoch {
            return;
        }
        self.context_pins.reads.remove(&message);
        cx.notify();
        if self.connection_generation() != generation {
            return;
        }
        let text = match result {
            Ok(whole) => context_pins::first_text(&whole).map(clip_line),
            Err(error) => {
                tracing::warn!(%error, "a pinned message could not be read");
                None
            }
        };
        self.context_pins.fetched.insert(message, text);
    }

    /// Goes to a pinned message (`handleReveal`): raises the focus request
    /// and leaves the card when it lies over the transcript.
    pub(crate) fn reveal_pinned_message(
        &mut self,
        message: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        self.request_message_focus(&session, message, window, cx);
        self.work_status_overlay = false;
        cx.notify();
    }

    /// Unpins `message` from the selected session. The row stays until the
    /// server confirms; a second press while one unpin runs does nothing.
    pub(crate) fn unpin_context_message(
        &mut self,
        message: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        if self.context_pins.unpinning.is_some() {
            return;
        }
        let key = self.mutation_key(&session);
        self.context_pins.unpinning = Some((key.clone(), message.clone()));
        self.queue_context_pin_change(key, PinChange::unpin(message.clone()), window, cx);
    }

    /// Pins the message under whose answer the button was pressed, or unpins
    /// it (`handleToggleContextPin`).
    pub(crate) fn toggle_context_pin(
        &mut self,
        session: &SessionId,
        message: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(session);
        let toggle = (key.clone(), message.clone());
        if self.context_pins.toggling.contains(&toggle) {
            return;
        }
        let Some(record) = self.session(session) else {
            return;
        };
        let pinned = context_pins::is_pinned(record, message);
        let held = self.open_sessions.get(session).and_then(|open| {
            open.transcript
                .message(open.transcript.row_of(message)?)
                .map(|held| (held.info.role, held.info.created))
        });
        // A message without a creation time cannot be placed in the
        // conversation.
        let Some((role, created)) = held.filter(|(_, created)| *created != 0) else {
            return;
        };
        let role = match role {
            MessageRole::User => PinRole::User,
            MessageRole::Assistant => PinRole::Assistant,
            _ => return,
        };
        let pin = (!pinned).then(|| ContextPin {
            id: message.clone(),
            created_at: PinTime::from_millis(created),
            role,
            session_id: Some(session.clone()),
        });
        self.context_pins.toggling.insert(toggle);
        self.queue_context_pin_change(key, PinChange::toggle(message.clone(), pin), window, cx);
    }

    /// Adds a change behind the session's earlier ones and starts it when
    /// none is in flight.
    fn queue_context_pin_change(
        &mut self,
        key: SessionKey,
        change: PinChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_pins
            .change_queue
            .entry(key.clone())
            .or_default()
            .push_back(change);
        self.pump_context_pin_changes(key, window, cx);
        cx.notify();
    }

    /// Sends the session's next change once the previous one was answered,
    /// so each starts from a read that already holds the one before it.
    fn pump_context_pin_changes(
        &mut self,
        key: SessionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context_pins.changing.contains(&key) {
            return;
        }
        let Some(change) = self
            .context_pins
            .change_queue
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
        else {
            self.context_pins.change_queue.remove(&key);
            return;
        };
        let session = key.session.clone();
        let request = change.clone();
        let (finished_key, finished_change) = (key.clone(), change.clone());
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    match &request.pin {
                        Some(pin) => client.pin_context_message(&session, pin).await,
                        None => {
                            client
                                .unpin_context_message(&session, &request.message)
                                .await
                        }
                    }
                })
            },
            move |this, result, window, cx| {
                this.finish_context_pin_change(finished_key, finished_change, result, window, cx)
            },
            window,
            cx,
        );
        if started {
            self.context_pins.changing.insert(key);
        } else {
            self.finish_context_pin_change(key, change, None, window, cx);
        }
    }

    /// Frees what the change held, shows the server's answer or says the
    /// failure, and starts the session's next change.
    pub(crate) fn finish_context_pin_change(
        &mut self,
        key: SessionKey,
        change: PinChange,
        result: Option<Result<MetadataFacts, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_pins.changing.remove(&key);
        match change.origin {
            Origin::Section => {
                if self
                    .context_pins
                    .unpinning
                    .as_ref()
                    .is_some_and(|(running, pin)| *running == key && *pin == change.message)
                {
                    self.context_pins.unpinning = None;
                }
            }
            Origin::Footer => {
                self.context_pins
                    .toggling
                    .remove(&(key.clone(), change.message.clone()));
            }
        }
        if key.runtime == self.runtime {
            let changed = matches!(result, Some(Ok(_)));
            let failure_key = match change.origin {
                Origin::Section => "chat.workStatus.pinned.unpinFailed",
                Origin::Footer => "chat.messageBody.actions.contextPinFailed",
            };
            self.apply_context_pin_result(key.session.clone(), result, failure_key, window, cx);
            // After the footer's change the reader keeps typing.
            if change.origin == Origin::Footer
                && changed
                && self.selected.as_ref() == Some(&key.session)
            {
                self.focus_composer(window, cx);
            }
        }
        self.pump_context_pin_changes(key, window, cx);
    }

    /// The server's metadata after a pin change, or the failure's notice.
    fn apply_context_pin_result(
        &mut self,
        session: SessionId,
        result: Option<Result<MetadataFacts, TransportError>>,
        failure_key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Some(Ok(metadata)) => self.apply_server_events(
                vec![ServerEvent::SessionPatched {
                    session_id: session,
                    patch: SessionPatch {
                        metadata: Some(metadata),
                        ..SessionPatch::default()
                    },
                }],
                window,
                cx,
            ),
            Some(Err(_)) | None => self.push_error(failure_key, None, window, cx),
        }
        cx.notify();
    }

    /// The section: a heading and a row per pin. Absent without pins.
    pub(crate) fn render_pinned_messages(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.selected.as_ref()?;
        let rows = self.pinned_rows();
        if rows.is_empty() {
            return None;
        }
        let strings = locale::strings(cx);
        let in_flight = self.message_focus.in_flight(session, Instant::now());
        let unpinning = self
            .context_pins
            .unpinning
            .as_ref()
            .map(|(_, message)| message);
        let heading = div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .mb(px(2.0))
            .px(px(4.0))
            .when(first_heading, |heading| {
                heading.padding_end(direction, px(28.0))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.foreground)
                    .child(strings.get("chat.workStatus.section.pinned")),
            );
        let rows = rows.into_iter().map(|row| {
            let busy = unpinning == Some(&row.id);
            let revealing = in_flight == Some(&row.id);
            self.render_pinned_row(row, busy, revealing, palette, direction, cx)
        });
        Some(
            div()
                .id("work-status-pinned")
                .role(Role::Group)
                .accessibility_id("work-status-pinned")
                .aria_label(strings.get("chat.workStatus.section.pinned"))
                .flex()
                .flex_col()
                .child(heading)
                .children(rows)
                .test_support()
                .into_any_element(),
        )
    }

    /// A 28 px row with two destinations: the pin unpins, the rest of the
    /// row goes to the message. The row's button lies under its content, so
    /// neither button holds the other.
    fn render_pinned_row(
        &self,
        row: PinnedRow,
        busy: bool,
        revealing: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let id = row.id;
        let reveal_id = id.clone();
        let reveal = on_activate(
            div()
                .id(SharedString::from(format!(
                    "work-status-pinned-reveal:{id}"
                )))
                .role(Role::Button)
                .accessibility_id(format!("work-status-pinned-reveal:{id}"))
                .aria_label(strings.get("chat.workStatus.pinned.reveal"))
                .tab_index(0)
                .absolute()
                .inset_0()
                .rounded(px(CONTROL_RADIUS))
                .cursor_pointer(),
            cx,
            move |this, window, cx| this.reveal_pinned_message(&reveal_id, window, cx),
        )
        .test_support();
        let unpin_id = id.clone();
        // The pin's box is 18 px around a 14 px glyph and takes 14 px of the
        // row (`-m-0.5 p-0.5`).
        let unpin = on_activate(
            div()
                .id(SharedString::from(format!("work-status-pinned-unpin:{id}")))
                .role(Role::Button)
                .accessibility_id(format!("work-status-pinned-unpin:{id}"))
                .aria_label(strings.get("chat.workStatus.pinned.unpin"))
                .tab_index(0)
                .flex_shrink_0()
                .m(px(-2.0))
                .p(px(2.0))
                .rounded(px(4.0))
                .map(|pin| {
                    if busy {
                        pin.opacity(0.4)
                            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    } else {
                        pin.opacity(0.7)
                            .cursor_pointer()
                            .hover(|style| style.opacity(1.0))
                    }
                })
                .child(glyph("pushpin-2-fill", GLYPH, palette.primary)),
            cx,
            move |this, window, cx| {
                // The press is the pin's, also while it is disabled.
                cx.stop_propagation();
                if !busy {
                    this.unpin_context_message(&unpin_id, window, cx);
                }
            },
        )
        .test_support();
        div()
            .relative()
            .h(px(ROW_HEIGHT))
            .px(px(4.0))
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .text_color(palette.muted_foreground)
            .child(reveal)
            .child(unpin)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .child(
                        row.text
                            .unwrap_or_else(|| strings.get("chat.workStatus.pinned.unavailable")),
                    ),
            )
            .when(revealing, |this| {
                this.child(glyph("loader-4", GLYPH, palette.muted_foreground))
            })
            .into_any_element()
    }

    /// Whether a pin change of `message` is in flight.
    pub(crate) fn context_pin_pending(&self, session: &SessionId, message: &MessageId) -> bool {
        self.context_pins
            .toggling
            .contains(&(self.mutation_key(session), message.clone()))
    }

    /// The pin button under an answer: muted and hollow at rest, filled in
    /// the info tone while its message is pinned, dimmed while its change is
    /// in flight.
    pub(crate) fn render_context_pin_button(
        &self,
        session: &SessionId,
        message: &MessageId,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let pinned = self
            .session(session)
            .is_some_and(|record| context_pins::is_pinned(record, message));
        let pending = self
            .context_pins
            .toggling
            .contains(&(self.mutation_key(session), message.clone()));
        let label = strings.get(if pinned {
            "chat.messageBody.actions.unpinContext"
        } else {
            "chat.messageBody.actions.pinContext"
        });
        let (icon, rest) = if pinned {
            ("pushpin-2-fill", palette.status_info)
        } else {
            ("pushpin-2", palette.muted_foreground)
        };
        let foreground = palette.foreground;
        let group = SharedString::from(format!("message-pin:{message}"));
        let (toggle_session, toggle_message) = (session.clone(), message.clone());
        on_activate(
            div()
                .id(group.clone())
                .group(group.clone())
                .role(Role::Button)
                .accessibility_id(group.clone())
                .aria_label(label)
                .aria_toggled(if pinned {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .size(px(FOOTER_BUTTON))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .map(|button| {
                    if pending {
                        button
                            .opacity(0.5)
                            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    } else {
                        button.cursor_pointer()
                    }
                })
                .child(glyph(icon, FOOTER_GLYPH, rest).when(!pending, |glyph| {
                    glyph.group_hover(group, move |style| style.text_color(foreground))
                })),
            cx,
            move |this, window, cx| {
                if !pending {
                    this.toggle_context_pin(&toggle_session, &toggle_message, window, cx);
                }
            },
        )
        .test_support()
        .into_any_element()
    }
}
