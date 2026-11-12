//! Search in the selected conversation (`ChatSearchBar.tsx`).
//!
//! Ctrl+F opens a bar at the transcript's top end while the server setting
//! `messageSearchEnabled` is on (its default is off, as on the web). The
//! query goes to the server's message index 200 ms after typing stops, in
//! pages of 200 hits, oldest first, up to five pages. The newest hit is the
//! current one; Enter steps to the next, Shift+Enter to the previous, both
//! wrapping around, and Escape closes the bar.
//!
//! Moving to a hit is a message-focus request ([`crate::message_focus`]), so
//! a hit before the loaded history is reached through the transcript's own
//! older-history loader and a message the server no longer has is reported.
//! Hits are those of the query's last search; a reply that streams in after
//! it is found by the next query.
//!
//! Every occurrence of the query's words in a hit's message is tinted, the
//! current hit's more strongly (`chatSearchHighlight.ts`). The tint is a
//! range highlight on the message's retained text views, set before each
//! frame for the views whose text or emphasis changed, and removed when the
//! bar closes. Reasoning is not drawn through those views, so a hit in
//! reasoning moves to its message without a tint.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use gpui_kit::base::text::{RangeHighlight, RenderedText, TextViewState};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, EntityId, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::SessionId;
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
/// `::highlight(oc-chat-search)`: the primary color at 30 %.
const HIT_TINT_ALPHA: f32 = 0.3;
/// The current hit's occurrences. The reference paints them in the solid
/// primary color under primary-foreground text; a range highlight leaves the
/// text's own color, so the fill stays translucent enough to read it.
const CURRENT_TINT_ALPHA: f32 = 0.6;

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

/// What one text view was last painted with.
struct Painted {
    text: WeakEntity<TextViewState>,
    /// The text the ranges were found in; a later parse is searched again.
    rendered: RenderedText,
    current: bool,
}

pub(crate) struct TranscriptSearch {
    session: SessionId,
    input: Entity<InputState>,
    state: SearchState,
    current: Option<usize>,
    /// The words of the query the hits answer, lowercased.
    terms: Vec<String>,
    pending: Option<Task<()>>,
    /// What had focus before the bar opened.
    return_focus: Option<FocusHandle>,
    /// The text views carrying this search's tint.
    painted: HashMap<EntityId, Painted>,
    _subscription: Subscription,
}

/// True when the query has a word the index can look up (`isSearchableQuery`).
pub(crate) fn is_searchable_query(query: &str) -> bool {
    query
        .split_whitespace()
        .any(|term| term.chars().count() >= MIN_TERM_LENGTH)
}

/// The query's words the index can match, lowercased (`toHighlightTerms`).
pub(crate) fn highlight_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|term| term.chars().count() >= MIN_TERM_LENGTH)
        .map(str::to_lowercase)
        .collect()
}

/// Every occurrence of `terms` in `text`, compared without case, as byte
/// ranges of `text` on character boundaries, in reading order. Occurrences
/// of one term never overlap. `terms` are lowercase.
pub(crate) fn term_ranges(text: &str, terms: &[String]) -> Vec<Range<usize>> {
    if text.is_empty() || terms.iter().all(String::is_empty) {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    if text.is_ascii() {
        // Lowercasing ASCII keeps every offset.
        let lower = text.to_ascii_lowercase();
        find_terms(&lower, terms, |range| ranges.push(range));
    } else {
        // Lowercasing may change a character's length, so each lowered
        // character remembers the range it came from.
        let mut lower = String::with_capacity(text.len());
        let mut sources: Vec<(usize, Range<usize>)> = Vec::new();
        for (start, character) in text.char_indices() {
            let source = start..start + character.len_utf8();
            for lowered in character.to_lowercase() {
                sources.push((lower.len(), source.clone()));
                lower.push(lowered);
            }
        }
        let source_of = |offset: usize| {
            let index = sources.partition_point(|(at, _)| *at <= offset);
            sources[index.saturating_sub(1)].1.clone()
        };
        find_terms(&lower, terms, |range| {
            ranges.push(source_of(range.start).start..source_of(range.end - 1).end)
        });
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges
}

/// Calls `found` with the range of each occurrence of each term in `lower`.
fn find_terms(lower: &str, terms: &[String], mut found: impl FnMut(Range<usize>)) {
    for term in terms.iter().filter(|term| !term.is_empty()) {
        let mut from = 0;
        while let Some(at) = lower[from..].find(term.as_str()) {
            let start = from + at;
            from = start + term.len();
            found(start..from);
        }
    }
}

fn clear_painted(painted: HashMap<EntityId, Painted>, cx: &mut App) {
    for (_, painted) in painted {
        if let Some(text) = painted.text.upgrade() {
            text.update(cx, |text, cx| text.clear_range_highlights(cx));
        }
    }
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
        self.begin_transcript_search(session, window, cx);
    }

    /// Opens the bar for `session` and focuses its field.
    fn begin_transcript_search(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A bar left open for another session gives its tint back first.
        self.close_transcript_search(false, window, cx);
        let return_focus = window.focused(cx);
        let placeholder = locale::strings(cx).get("chat.search.placeholder");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Change => this.transcript_search_changed(window, cx),
                InputEvent::PressEnter { shift, .. } => {
                    this.step_transcript_search(if *shift { -1 } else { 1 }, window, cx)
                }
                _ => {}
            });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.transcript_search = Some(TranscriptSearch {
            session,
            input,
            state: SearchState::Idle,
            current: None,
            terms: Vec::new(),
            pending: None,
            return_focus,
            painted: HashMap::new(),
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the bar and removes its tint. The transcript stays where the
    /// last hit left it, as on the web. `restore_focus` returns the keyboard
    /// to where it was before the bar opened, or to the composer.
    pub(crate) fn close_transcript_search(
        &mut self,
        restore_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.transcript_search.take() else {
            return;
        };
        clear_painted(search.painted, cx);
        if restore_focus {
            match search.return_focus {
                Some(focus) => window.focus(&focus, cx),
                None => self.focus_composer(window, cx),
            }
        }
        cx.notify();
    }

    /// Before each frame: the search belongs to the conversation it was
    /// opened in, and the views of its hits carry its tint.
    pub(crate) fn sync_transcript_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.transcript_search.as_ref() else {
            return;
        };
        if self.selected.as_ref() != Some(&search.session) {
            self.close_transcript_search(false, window, cx);
            return;
        }
        self.paint_search_hits(cx);
    }

    /// Tints the query's words in every loaded hit, the current hit's more
    /// strongly. A view whose text and emphasis are as last painted is left
    /// alone, so an unchanged frame searches nothing.
    fn paint_search_hits(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        let mut wanted: HashMap<EntityId, (Entity<TextViewState>, bool)> = HashMap::new();
        if let SearchState::Ready(hits) = &search.state
            && !search.terms.is_empty()
            && let Some(open) = self.open_sessions.get(&search.session)
        {
            let current = search
                .current
                .and_then(|current| hits.get(current))
                .filter(|hit| !hit.reasoning)
                .map(|hit| &hit.message);
            let selection = open.selection.read(cx);
            for hit in hits.iter().filter(|hit| !hit.reasoning) {
                if open.transcript.row_of(&hit.message).is_none() {
                    continue;
                }
                let is_current = current == Some(&hit.message);
                for text in selection.text_states(&hit.message, cx) {
                    let entry = wanted.entry(text.entity_id()).or_insert((text, false));
                    entry.1 |= is_current;
                }
            }
        }
        let primary = cx.global::<Palette>().primary;
        let stale: Vec<EntityId> = search
            .painted
            .keys()
            .filter(|id| !wanted.contains_key(*id))
            .copied()
            .collect();
        for id in stale {
            if let Some(text) = search
                .painted
                .remove(&id)
                .and_then(|painted| painted.text.upgrade())
            {
                text.update(cx, |text, cx| text.clear_range_highlights(cx));
            }
        }
        for (id, (text, current)) in wanted {
            let rendered = text.read(cx).rendered_text();
            if search
                .painted
                .get(&id)
                .is_some_and(|painted| painted.current == current && painted.rendered == rendered)
            {
                continue;
            }
            let fill = primary.opacity(if current {
                CURRENT_TINT_ALPHA
            } else {
                HIT_TINT_ALPHA
            });
            let highlights: Vec<RangeHighlight> = term_ranges(rendered.as_str(), &search.terms)
                .into_iter()
                .map(|range| RangeHighlight::new(range, fill))
                .collect();
            if let Err(error) =
                text.update(cx, |text, cx| text.set_range_highlights(highlights, cx))
            {
                tracing::warn!(%error, "search matches could not be painted");
            }
            search.painted.insert(
                id,
                Painted {
                    text: text.downgrade(),
                    rendered,
                    current,
                },
            );
        }
    }

    fn transcript_search_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        search.pending = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.run_transcript_search(session, query, window, cx)
            });
        }));
    }

    fn run_transcript_search(
        &mut self,
        session: SessionId,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let terms = highlight_terms(&query);
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
        search.pending = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_transcript_search(
                    &session,
                    terms,
                    result.unwrap_or(SearchState::Error),
                    window,
                    cx,
                );
            });
        }));
        cx.notify();
    }

    /// Takes a query's answer for the bar still open on `session` and moves
    /// to its newest hit: the reader is usually near the end.
    fn finish_transcript_search(
        &mut self,
        session: &SessionId,
        terms: Vec<String>,
        state: SearchState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self
            .transcript_search
            .as_mut()
            .filter(|search| search.session == *session)
        else {
            return;
        };
        search.current = match &state {
            SearchState::Ready(hits) => hits.len().checked_sub(1),
            _ => None,
        };
        search.state = state;
        search.terms = terms;
        self.reveal_search_hit(window, cx);
        cx.notify();
    }

    fn step_transcript_search(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        let SearchState::Ready(hits) = &search.state else {
            return;
        };
        if hits.is_empty() {
            return;
        }
        search.current = step(search.current, hits.len(), delta);
        self.reveal_search_hit(window, cx);
        cx.notify();
    }

    /// Asks the transcript to show the current hit's message. The request
    /// loads older history when the message lies before the loaded pages.
    fn reveal_search_hit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, message)) = self.transcript_search.as_ref().and_then(|search| {
            let SearchState::Ready(hits) = &search.state else {
                return None;
            };
            let hit = hits.get(search.current?)?;
            Some((search.session.clone(), hit.message.clone()))
        }) else {
            return;
        };
        self.request_message_focus(&session, &message, window, cx);
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
                    .render(palette, cx, move |this, window, cx| {
                        this.step_transcript_search(delta, window, cx)
                    }),
            )
        };
        let bar = div()
            .id("transcript-search")
            .role(Role::Search)
            .accessibility_id("transcript-search")
            // The input passes Escape on when it has nothing of its own to
            // dismiss.
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                this.close_transcript_search(true, window, cx)
            }))
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
                .render(palette, cx, |this, window, cx| {
                    this.close_transcript_search(true, window, cx)
                }),
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
    use std::time::Instant;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, TestAppContext};
    use openchamber_native_core::ids::MessageId;
    use openchamber_native_core::transcript::Role as MessageRole;

    use super::*;
    use crate::workspace_tests::open;

    fn hit(message: &MessageId) -> SearchHit {
        SearchHit {
            message: message.clone(),
            reasoning: false,
        }
    }

    fn update<T>(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> T,
    ) -> T {
        cx.update_window(window, |_, window, cx| {
            let result = workspace.update(cx, |workspace, cx| f(workspace, window, cx));
            window.render_frame(cx);
            result
        })
        .expect("window is open")
    }

    /// Selects a root session and opens the bar on it. Returns the session,
    /// its prompts with text, oldest first, and a word of the first one.
    fn open_search(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> (SessionId, Vec<MessageId>, String) {
        update(window, workspace, cx, |this, window, cx| {
            let session = this
                .sessions
                .active()
                .iter()
                .find(|record| record.parent_id.is_none() && record.directory.is_some())
                .expect("a root session")
                .id
                .clone();
            this.select_session(session.clone(), window, cx);
            let open = this.open_sessions.get(&session).expect("opened");
            let prompts: Vec<(MessageId, String)> = open
                .transcript
                .messages()
                .iter()
                .filter(|message| matches!(message.info.role, MessageRole::User))
                .map(|message| (message.info.id.clone(), message.plain_text()))
                .filter(|(_, text)| is_searchable_query(text))
                .collect();
            let word = prompts
                .first()
                .and_then(|(_, text)| highlight_terms(text).into_iter().next())
                .expect("a prompt with a searchable word");
            this.begin_transcript_search(session.clone(), window, cx);
            let ids = prompts.into_iter().map(|(id, _)| id).collect();
            (session, ids, word)
        })
    }

    #[test]
    fn highlight_terms_are_the_searchable_words_lowercased() {
        assert_eq!(
            highlight_terms("  Foo ab BAR\tÜber "),
            ["foo", "bar", "über"]
        );
        assert!(highlight_terms("ab cd").is_empty());
        assert_eq!(highlight_terms("日本語 の"), ["日本語"]);
    }

    #[test]
    fn term_ranges_find_every_occurrence_without_case() {
        let terms = highlight_terms("foo bar");
        assert_eq!(
            term_ranges("Foo bar FOObar", &terms),
            [0..3, 4..7, 8..11, 11..14]
        );
        assert_eq!(term_ranges("aaaaaa", &highlight_terms("aaa")), [0..3, 3..6]);
        assert!(term_ranges("nothing here", &terms).is_empty());
        assert!(term_ranges("", &terms).is_empty());
        assert!(term_ranges("foo", &[]).is_empty());
    }

    #[test]
    fn term_ranges_stay_on_character_boundaries_in_multi_byte_text() {
        for (text, query, expected) in [
            ("Größe und GRÖSSE", "größe", vec!["Größe"]),
            (
                "日本語のテキスト、日本語",
                "日本語",
                vec!["日本語", "日本語"],
            ),
            ("héllo 🎉 HÉLLO", "héllo", vec!["héllo", "HÉLLO"]),
            // `İ` lowercases to two characters, three bytes for its two.
            ("İstanbul İSTANBUL", "stanbul", vec!["stanbul", "STANBUL"]),
            ("İİİ abc", "abc", vec!["abc"]),
        ] {
            let ranges = term_ranges(text, &highlight_terms(query));
            let found: Vec<&str> = ranges
                .iter()
                .map(|range| {
                    assert!(
                        text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                        "{range:?} splits a character of {text:?}"
                    );
                    &text[range.clone()]
                })
                .collect();
            assert_eq!(found, expected, "{query:?} in {text:?}");
        }
    }

    #[test]
    fn a_match_made_by_lowercasing_covers_the_whole_source_character() {
        // The lowered `İ` is `i` and a combining dot; a term matching only
        // the `i` still paints the whole `İ`.
        let text = "xİy";
        let ranges = term_ranges(text, &["i".to_owned()]);
        assert_eq!(ranges, [1..3]);
        assert_eq!(&text[ranges[0].clone()], "İ");
    }

    #[gpui_kit::test]
    fn stepping_moves_through_the_hits_and_wraps(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let (session, prompts, word) = open_search(window, &workspace, cx);
        let hits: Vec<SearchHit> = prompts.iter().map(hit).collect();
        let total = hits.len();
        update(window, &workspace, cx, |this, window, cx| {
            this.finish_transcript_search(
                &session,
                vec![word.clone()],
                SearchState::Ready(hits),
                window,
                cx,
            );
        });
        let current = |cx: &mut TestAppContext| {
            cx.read(|cx| {
                workspace
                    .read(cx)
                    .transcript_search
                    .as_ref()
                    .and_then(|search| search.current)
            })
        };
        assert_eq!(current(cx), Some(total - 1), "the newest hit is current");
        update(window, &workspace, cx, |this, window, cx| {
            this.step_transcript_search(1, window, cx);
        });
        assert_eq!(current(cx), Some(0), "next wraps to the oldest hit");
        update(window, &workspace, cx, |this, window, cx| {
            this.step_transcript_search(-1, window, cx);
        });
        assert_eq!(current(cx), Some(total - 1), "previous wraps back");
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(
                state.message_focus.standing(Instant::now()).is_none(),
                "a loaded hit is shown at once"
            );
            let strings = locale::strings(cx);
            let search = state.transcript_search.as_ref().expect("open");
            assert_eq!(
                status_text(&search.state, search.current, strings).as_ref(),
                format!("{total} of {total}")
            );
        });
    }

    #[gpui_kit::test]
    fn a_hit_outside_the_loaded_history_goes_through_message_focus(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let (session, prompts, word) = open_search(window, &workspace, cx);
        let unloaded = MessageId::from("msg_before_the_loaded_pages");
        let hits = vec![hit(&unloaded), hit(&prompts[0])];
        update(window, &workspace, cx, |this, window, cx| {
            this.finish_transcript_search(
                &session,
                vec![word.clone()],
                SearchState::Ready(hits),
                window,
                cx,
            );
            this.session_management.notice = None;
            // Previous: the hit the transcript does not hold.
            this.step_transcript_search(-1, window, cx);
        });
        cx.read(|cx| {
            let state = workspace.read(cx);
            let search = state.transcript_search.as_ref().expect("still open");
            assert_eq!(search.current, Some(0));
            // Synthetic data has no older page and no server to ask, so the
            // focus request ends by saying the message is not there.
            assert!(state.message_focus.standing(Instant::now()).is_none());
            assert_eq!(
                state.session_management.notice,
                Some(locale::strings(cx).get("chat.messageLink.notFound"))
            );
        });
    }

    #[gpui_kit::test]
    fn hits_are_tinted_and_closing_removes_the_tint(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let (session, prompts, word) = open_search(window, &workspace, cx);
        let hits: Vec<SearchHit> = prompts.iter().take(2).map(hit).collect();
        let newest = hits.last().expect("a hit").message.clone();
        update(window, &workspace, cx, |this, window, cx| {
            this.finish_transcript_search(
                &session,
                vec![word.clone()],
                SearchState::Ready(hits.clone()),
                window,
                cx,
            );
            this.sync_transcript_search(window, cx);
        });
        let texts = cx.read(|cx| {
            let state = workspace.read(cx);
            let open = state.open_sessions.get(&session).expect("opened");
            let selection = open.selection.read(cx);
            let search = state.transcript_search.as_ref().expect("open");
            let mut texts = Vec::new();
            for hit in &hits {
                for text in selection.text_states(&hit.message, cx) {
                    let painted = search
                        .painted
                        .get(&text.entity_id())
                        .expect("every view of a hit is painted");
                    assert_eq!(painted.current, hit.message == newest);
                    texts.push(text);
                }
            }
            assert_eq!(search.painted.len(), texts.len(), "only hits are painted");
            texts
        });
        assert!(!texts.is_empty());
        // The older hit becomes current: its views are painted again.
        update(window, &workspace, cx, |this, window, cx| {
            this.step_transcript_search(-1, window, cx);
            this.sync_transcript_search(window, cx);
        });
        cx.read(|cx| {
            let state = workspace.read(cx);
            let search = state.transcript_search.as_ref().expect("open");
            let selection = state
                .open_sessions
                .get(&session)
                .expect("opened")
                .selection
                .read(cx);
            for hit in &hits {
                for text in selection.text_states(&hit.message, cx) {
                    assert_eq!(
                        search.painted[&text.entity_id()].current,
                        hit.message != newest || hits.len() == 1
                    );
                }
            }
        });
        // A query without an answer yet tints nothing.
        update(window, &workspace, cx, |this, window, cx| {
            if let Some(search) = this.transcript_search.as_mut() {
                search.state = SearchState::Loading;
            }
            this.sync_transcript_search(window, cx);
        });
        assert!(cx.read(|cx| {
            workspace
                .read(cx)
                .transcript_search
                .as_ref()
                .is_some_and(|search| search.painted.is_empty())
        }));
        update(window, &workspace, cx, |this, window, cx| {
            this.close_transcript_search(true, window, cx);
        });
        assert!(cx.read(|cx| workspace.read(cx).transcript_search.is_none()));
    }

    #[gpui_kit::test]
    fn the_search_closes_with_its_conversation_and_drops_a_late_answer(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let (session, prompts, word) = open_search(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            let other = this
                .sessions
                .active()
                .iter()
                .find(|record| {
                    record.id != session && record.parent_id.is_none() && record.directory.is_some()
                })
                .expect("a second root session")
                .id
                .clone();
            this.select_session(other, window, cx);
        });
        update(window, &workspace, cx, |this, window, cx| {
            this.sync_transcript_search(window, cx);
            assert!(this.transcript_search.is_none());
            // The answer of the closed bar's query changes nothing.
            this.finish_transcript_search(
                &session,
                vec![word.clone()],
                SearchState::Ready(vec![hit(&prompts[0])]),
                window,
                cx,
            );
            assert!(this.transcript_search.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_failed_or_empty_search_says_so_and_steps_nowhere(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let (session, _, word) = open_search(window, &workspace, cx);
        for (state, text) in [
            (SearchState::Error, "chat.search.error"),
            (SearchState::Unavailable, "chat.search.unavailable"),
            (SearchState::Ready(Vec::new()), "chat.search.noResults"),
        ] {
            update(window, &workspace, cx, |this, window, cx| {
                this.finish_transcript_search(&session, vec![word.clone()], state, window, cx);
                this.step_transcript_search(1, window, cx);
            });
            cx.read(|cx| {
                let strings = locale::strings(cx);
                let search = workspace.read(cx).transcript_search.as_ref().expect("open");
                assert_eq!(search.current, None);
                assert_eq!(
                    status_text(&search.state, search.current, strings),
                    strings.get(text)
                );
            });
        }
    }

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
        let hit = hit(&MessageId::from("m"));
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
