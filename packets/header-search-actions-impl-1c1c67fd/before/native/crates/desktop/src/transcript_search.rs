//! Search in the selected conversation (`ChatSearchBar.tsx`).
//!
//! Ctrl+F opens a bar at the transcript's top end while the server setting
//! `messageSearchEnabled` is on (its default is off, as on the web). The
//! query goes to the server's message index 200 ms after typing stops, in
//! pages of 200 hits, oldest first, up to five pages. The newest hit is the
//! current one; Enter steps to the next, Shift+Enter to the previous, and
//! Escape closes the bar. Moving to a hit scrolls to its message when the
//! transcript holds it. Highlighting the matched words and loading history
//! older than the loaded pages to reach a hit are not ported.

use std::time::Duration;

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::message_search::{SearchHit, SearchPage};

use crate::chrome::{IconButton, glyph};
use crate::elevation;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const DEBOUNCE: Duration = Duration::from_millis(200);
const PAGE_SIZE: usize = 200;
const MAX_PAGES: usize = 5;
/// The index looks words up from three characters (`MIN_QUERY_TERM_LENGTH`).
const MIN_TERM_LENGTH: usize = 3;

/// Where a query stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SearchState {
    Idle,
    TooShort,
    Loading,
    Ready(Vec<SearchHit>),
    Error,
    Unavailable,
}

pub(crate) struct TranscriptSearch {
    session: SessionId,
    input: Entity<InputState>,
    state: SearchState,
    current: Option<usize>,
    pending: Option<Task<()>>,
    _subscription: Subscription,
}

/// True when the query has a word the index can look up (`isSearchableQuery`).
pub(crate) fn is_searchable_query(query: &str) -> bool {
    query
        .split_whitespace()
        .any(|term| term.chars().count() >= MIN_TERM_LENGTH)
}

/// The hit after stepping `delta` from `current`, wrapping around.
pub(crate) fn step(current: Option<usize>, total: usize, delta: isize) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let from = current.map_or(0, |current| current as isize);
    Some((from + delta).rem_euclid(total as isize) as usize)
}

/// The status line: the position, or what the query is waiting on.
pub(crate) fn status_text(
    state: &SearchState,
    current: Option<usize>,
    strings: &Strings,
) -> SharedString {
    match state {
        SearchState::Idle => SharedString::default(),
        SearchState::TooShort => strings.get("chat.search.tooShort"),
        SearchState::Loading => strings.get("chat.search.searching"),
        SearchState::Error => strings.get("chat.search.error"),
        SearchState::Unavailable => strings.get("chat.search.unavailable"),
        SearchState::Ready(hits) if hits.is_empty() => strings.get("chat.search.noResults"),
        SearchState::Ready(hits) => strings.format(
            "chat.search.position",
            &[
                ("current", &(current.unwrap_or(0) + 1).to_string()),
                ("total", &hits.len().to_string()),
            ],
        ),
    }
}

impl Workspace {
    fn search_enabled(&self) -> bool {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.message_search_enabled)
            .unwrap_or(false)
    }

    /// Ctrl+F: opens the bar for the selected session, or focuses it again
    /// and selects its query.
    pub(crate) fn open_transcript_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        if !self.search_enabled() {
            return;
        }
        if let Some(search) = self
            .transcript_search
            .as_ref()
            .filter(|search| search.session == session)
        {
            let input = search.input.clone();
            input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
            return;
        }
        let placeholder = locale::strings(cx).get("chat.search.placeholder");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
            InputEvent::Change => this.transcript_search_changed(cx),
            InputEvent::PressEnter { shift, .. } => {
                this.step_transcript_search(if *shift { -1 } else { 1 }, cx)
            }
            _ => {}
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.transcript_search = Some(TranscriptSearch {
            session,
            input,
            state: SearchState::Idle,
            current: None,
            pending: None,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(crate) fn close_transcript_search(&mut self, cx: &mut Context<Self>) {
        if self.transcript_search.take().is_some() {
            cx.notify();
        }
    }

    fn transcript_search_changed(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        let query = search.input.read(cx).value().trim().to_owned();
        if query.is_empty() {
            search.pending = None;
            search.state = SearchState::Idle;
            search.current = None;
            cx.notify();
            return;
        }
        if !is_searchable_query(&query) {
            search.pending = None;
            search.state = SearchState::TooShort;
            search.current = None;
            cx.notify();
            return;
        }
        let session = search.session.clone();
        search.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.run_transcript_search(session, query, cx)
            });
        }));
    }

    fn run_transcript_search(&mut self, session: SessionId, query: String, cx: &mut Context<Self>) {
        let Some(client) = self
            .connection
            .as_ref()
            .and_then(|connection| connection.client.clone())
        else {
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let request_session = session.clone();
        let task = connection.network.run(async move {
            let mut hits = Vec::new();
            let mut before: Option<String> = None;
            for _ in 0..MAX_PAGES {
                match client
                    .search_session_messages(&request_session, &query, before.as_deref(), PAGE_SIZE)
                    .await?
                {
                    SearchPage::Unavailable => return Ok(SearchState::Unavailable),
                    SearchPage::QueryTooShort => return Ok(SearchState::TooShort),
                    SearchPage::Hits { hits: page, next } => {
                        hits.extend(page);
                        match next {
                            Some(next) => before = Some(next),
                            None => break,
                        }
                    }
                }
            }
            Ok::<_, TransportError>(SearchState::Ready(hits))
        });
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        search.state = SearchState::Loading;
        search.pending = Some(cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update(cx, |this, cx| {
                let Some(search) = this
                    .transcript_search
                    .as_mut()
                    .filter(|search| search.session == session)
                else {
                    return;
                };
                search.state = result.unwrap_or(SearchState::Error);
                // Start at the newest hit: the reader is usually near the end.
                search.current = match &search.state {
                    SearchState::Ready(hits) => hits.len().checked_sub(1),
                    _ => None,
                };
                this.reveal_search_hit(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn step_transcript_search(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        let SearchState::Ready(hits) = &search.state else {
            return;
        };
        search.current = step(search.current, hits.len(), delta);
        self.reveal_search_hit(cx);
        cx.notify();
    }

    /// Scrolls the current hit's message into view when the transcript
    /// holds it.
    fn reveal_search_hit(&mut self, _cx: &mut Context<Self>) {
        let Some(search) = self.transcript_search.as_ref() else {
            return;
        };
        let SearchState::Ready(hits) = &search.state else {
            return;
        };
        let Some(hit) = search.current.and_then(|current| hits.get(current)) else {
            return;
        };
        let message: &MessageId = &hit.message;
        if let Some(open) = self.open_sessions.get(&search.session)
            && let Some(row) = open.transcript.row_of(message)
        {
            open.list.scroll_to_reveal_item(row);
        }
    }

    /// The bar at the transcript's top end, for the selected session only.
    pub(crate) fn render_transcript_search(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let search = self.transcript_search.as_ref()?;
        if self.selected.as_ref() != Some(&search.session) {
            return None;
        }
        let strings = locale::strings(cx);
        let has_hits = matches!(&search.state, SearchState::Ready(hits) if !hits.is_empty());
        let status = status_text(&search.state, search.current, strings);
        // Without hits the steps do nothing and show dimmed.
        let step_button = |id: &str, icon: &'static str, label: &str, delta: isize| {
            div().when(!has_hits, |this| this.opacity(0.5)).child(
                IconButton::new(id.to_owned(), icon, strings.get(label))
                    .size(24.0, 16.0)
                    .render(palette, cx, move |this, _, cx| {
                        this.step_transcript_search(delta, cx)
                    }),
            )
        };
        let bar = div()
            .id("transcript-search")
            .role(Role::Search)
            .accessibility_id("transcript-search")
            // The input passes Escape on when it has nothing of its own to
            // dismiss.
            .on_action(cx.listener(|this, _: &Escape, _, cx| this.close_transcript_search(cx)))
            .flex()
            .flex_row_from(direction)
            .items_center()
            .gap_1()
            .py_1()
            .padding_start(direction, px(10.0))
            .padding_end(direction, px(4.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(palette.border)
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .child(glyph("search", 14.0, palette.muted_foreground))
            .child(
                div().w(px(192.0)).h(px(28.0)).child(
                    Input::new(&search.input)
                        .appearance(false)
                        .bordered(false)
                        .text_size(rems(crate::transcript_view::META_SIZE)),
                ),
            )
            .child(
                div()
                    .id("transcript-search-status")
                    .role(Role::Status)
                    .accessibility_id("transcript-search-status")
                    .aria_label(status.clone())
                    .min_w(px(72.0))
                    .flex_shrink_0()
                    .text_size(rems(0.6875))
                    .text_color(palette.muted_foreground)
                    .child(status),
            )
            .child(step_button(
                "transcript-search-previous",
                "arrow-up-s",
                "chat.search.previous",
                -1,
            ))
            .child(step_button(
                "transcript-search-next",
                "arrow-down-s",
                "chat.search.next",
                1,
            ))
            .child(
                IconButton::new(
                    "transcript-search-close",
                    "close",
                    strings.get("chat.search.close"),
                )
                .size(24.0, 16.0)
                .render(palette, cx, |this, _, cx| this.close_transcript_search(cx)),
            );
        Some(
            div()
                .absolute()
                .top_2()
                .inset_end(direction, px(12.0))
                .child(bar)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_need_a_three_character_word() {
        assert!(!is_searchable_query("ab cd"));
        assert!(is_searchable_query("ab cde"));
        assert!(is_searchable_query("日本語"));
        assert!(!is_searchable_query("   "));
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        assert_eq!(step(Some(2), 3, 1), Some(0));
        assert_eq!(step(Some(0), 3, -1), Some(2));
        assert_eq!(step(None, 3, 1), Some(1));
        assert_eq!(step(Some(0), 0, 1), None);
    }

    #[test]
    fn the_status_names_the_position_or_the_wait() {
        let strings = Strings::load("en");
        let hit = SearchHit {
            message: MessageId::from("m"),
            reasoning: false,
        };
        assert_eq!(
            status_text(
                &SearchState::Ready(vec![hit.clone(), hit]),
                Some(1),
                &strings
            )
            .as_ref(),
            "2 of 2"
        );
        assert_eq!(
            status_text(&SearchState::Ready(Vec::new()), None, &strings).as_ref(),
            "No results"
        );
        assert_eq!(
            status_text(&SearchState::TooShort, None, &strings).as_ref(),
            "3+ characters"
        );
        assert_eq!(status_text(&SearchState::Idle, None, &strings).as_ref(), "");
    }
}
