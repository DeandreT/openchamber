//! A finished `websearch` call as a list of hits (`WebSearchResults.tsx`):
//! the title, the site and publish date, and two lines of the snippet. A hit
//! opens where a link in the text opens. Every hit carries the globe: the
//! native client does not fetch a site's own icon.
//!
//! The result is parsed, and each hit's snippet and date written, once per
//! result: an opened row keeps them in a [`SearchCache`] and a frame only
//! draws them.

use std::rc::Rc;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Utc};
use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::PartId;
use openchamber_native_core::message_time::DateFormats;
use openchamber_native_core::tool_shells::web_search::{self, WebSearchOutput, WebSearchResult};

use super::MICRO_SIZE;
use crate::activation::on_activate;
use crate::chrome::glyph;
use crate::locale::{Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::markdown_links::open_user_link;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;
use crate::workspace::Workspace;

/// What a finished search draws.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SearchView {
    /// OpenCode's "No search results found." answer.
    Empty,
    Hits(Vec<Hit>),
}

/// One hit as its card writes it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Hit {
    url: SharedString,
    /// The title, or the host when OpenCode had none.
    title: SharedString,
    host: SharedString,
    /// The local publish day in the locale's month names.
    published: Option<SharedString>,
    /// The snippet on one line.
    snippet: Option<SharedString>,
}

/// One opened row's parsed search result.
///
/// The result text is identified by where it is held and how long it is. A
/// finished call's result is replaced only by a whole read of the same call,
/// which holds its text elsewhere and is parsed again; dates also follow the
/// locale they were written in.
#[derive(Debug, Default)]
pub(super) struct SearchCache {
    source: Option<Source>,
    view: Option<Rc<SearchView>>,
}

#[derive(Debug, PartialEq, Eq)]
struct Source {
    address: usize,
    length: usize,
    locale: String,
}

impl SearchCache {
    /// The hits of `output`, parsed on the first call for this text and
    /// locale and handed back unchanged after that. `None` for a result in
    /// another shape, which the row prints as text.
    pub(super) fn refresh(&mut self, output: &str, strings: &Strings) -> Option<Rc<SearchView>> {
        let unchanged = self.source.as_ref().is_some_and(|source| {
            source.address == output.as_ptr() as usize
                && source.length == output.len()
                && source.locale == strings.locale()
        });
        if !unchanged {
            self.source = Some(Source {
                address: output.as_ptr() as usize,
                length: output.len(),
                locale: strings.locale().to_owned(),
            });
            self.view =
                web_search::parse(output).map(|found| Rc::new(view(found, strings.dates())));
        }
        self.view.clone()
    }
}

fn view(found: WebSearchOutput, dates: &DateFormats) -> SearchView {
    match found {
        WebSearchOutput::Empty => SearchView::Empty,
        WebSearchOutput::Results(results) => SearchView::Hits(
            results
                .into_iter()
                .map(|result| hit(result, dates))
                .collect(),
        ),
    }
}

fn hit(result: WebSearchResult, dates: &DateFormats) -> Hit {
    let snippet = result
        .snippet
        .as_deref()
        .map(|snippet| snippet.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|snippet| !snippet.is_empty());
    let published = result
        .published
        .as_deref()
        .and_then(|published| published_date(published, dates));
    Hit {
        title: result.title.unwrap_or_else(|| result.host.clone()).into(),
        url: result.url.into(),
        host: result.host.into(),
        published: published.map(SharedString::from),
        snippet: snippet.map(SharedString::from),
    }
}

pub(super) fn web_search_results(
    part: &PartId,
    found: &SearchView,
    provider: Option<&str>,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let id: SharedString = format!("tool:{part}:search-results").into();
    let column = div()
        .id(ElementId::Name(id.clone()))
        .role(Role::Group)
        .accessibility_id(id)
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1();
    let column = match found {
        SearchView::Empty => column.child(
            div()
                .px_2()
                .text_size(rems(META_SIZE))
                .text_color(palette.muted_foreground)
                .child(strings.get("chat.webSearch.noResults")),
        ),
        SearchView::Hits(hits) => {
            let mut list = div().flex().flex_col().gap(px(2.0));
            for (index, hit) in hits.iter().enumerate() {
                list = list.child(hit_card(
                    format!("tool:{part}:search-result:{index}").into(),
                    hit,
                    palette,
                    direction,
                    cx,
                ));
            }
            column.child(list)
        }
    };
    column
        .when_some(provider, |this, provider| {
            this.child(
                div()
                    .px_2()
                    .text_size(rems(MICRO_SIZE))
                    .text_color(palette.muted_foreground)
                    .child(strings.format("chat.webSearch.provider", &[("provider", provider)])),
            )
        })
        .test_support()
        .into_any_element()
}

fn hit_card(
    id: SharedString,
    hit: &Hit,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let hover = palette.hover.opacity(0.3);
    let url = hit.url.clone();
    let card = div()
        .id(ElementId::Name(id.clone()))
        .role(Role::Link)
        .accessibility_id(id)
        .aria_label(hit.title.clone())
        .flex()
        .flex_row_from(direction)
        .min_w_0()
        .gap_2()
        .px_2()
        .py(px(6.0))
        .rounded(px(8.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        // A press here is the hit's, never a fold of the row around it.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .mt_1()
                .size(px(14.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(glyph("global", 14.0, palette.muted_foreground)),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_size(rems(META_SIZE))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.foreground)
                        .child(hit.title.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row_from(direction)
                        .items_center()
                        .min_w_0()
                        .gap(px(6.0))
                        .text_size(rems(MICRO_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(div().min_w_0().truncate().child(hit.host.clone()))
                        .when_some(hit.published.clone(), |this, published| {
                            this.child(div().flex_shrink_0().child(published))
                        }),
                )
                .when_some(hit.snippet.clone(), |this, snippet| {
                    this.child(
                        div()
                            .mt(px(2.0))
                            .text_size(rems(META_SIZE))
                            .text_color(palette.muted_foreground)
                            .whitespace_normal()
                            .line_clamp(2)
                            .child(snippet),
                    )
                }),
        );
    on_activate(card, cx, move |_, window, cx| {
        let url = url.clone();
        // Link routing may update the workspace, which this action runs inside.
        window.defer(cx, move |window, cx| open_user_link(&url, window, cx));
    })
    .test_support()
    .into_any_element()
}

/// The local day a hit was published, as short month, day and year; `None`
/// for a timestamp that does not read as a date.
fn published_date(published: &str, dates: &DateFormats) -> Option<String> {
    let moment: DateTime<Utc> = match DateTime::parse_from_rfc3339(published) {
        Ok(moment) => moment.with_timezone(&Utc),
        // A bare date is midnight UTC, as `new Date("2026-01-05")` reads it.
        Err(_) => NaiveDate::parse_from_str(published, "%Y-%m-%d")
            .ok()?
            .and_time(NaiveTime::MIN)
            .and_utc(),
    };
    let day = moment.with_timezone(&Local).date_naive();
    Some(written_date(day, dates))
}

fn written_date(day: NaiveDate, dates: &DateFormats) -> String {
    let month = dates
        .months
        .get(day.month0() as usize)
        .map_or("", String::as_str);
    format!("{month} {}, {}", day.day(), day.year())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUND: &str = "## [Example](https://example.org/)\nPublished: 2026-01-05T12:00:00.000Z\n\nAn  example\nsite.\n\n## [https://second.example/a](https://second.example/a)";

    #[test]
    fn dates_write_the_short_month_day_and_year() {
        let dates = Strings::load("en");
        let day = NaiveDate::from_ymd_opt(2026, 1, 5).expect("a real day");
        assert_eq!(written_date(day, dates.dates()), "Jan 5, 2026");
    }

    #[test]
    fn timestamps_that_are_not_dates_show_nothing() {
        let dates = Strings::load("en");
        for unreadable in ["", "yesterday", "2026-13-40", "12345"] {
            assert_eq!(published_date(unreadable, dates.dates()), None);
        }
        // Noon UTC is the same calendar day in every zone from -11 to +11.
        let written = published_date("2026-01-05T12:00:00.000Z", dates.dates())
            .expect("an ISO timestamp is a date");
        assert!(written.ends_with("2026"), "{written}");
        assert!(written.starts_with("Jan "), "{written}");
        assert!(published_date("2026-01-05", dates.dates()).is_some());
    }

    #[test]
    fn an_unchanged_result_is_parsed_once() {
        let strings = Strings::load("en");
        // One held text, as a transcript holds a call's result across frames.
        let held = FOUND.to_owned();
        let mut cache = SearchCache::default();
        let first = cache
            .refresh(&held, &strings)
            .expect("the result reads as hits");
        let SearchView::Hits(hits) = first.as_ref() else {
            panic!("expected hits, got {first:?}");
        };
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title.as_ref(), "Example");
        assert_eq!(
            hits[0].snippet.as_deref(),
            Some("An example site."),
            "the snippet is collapsed when the result is parsed"
        );
        assert!(hits[0].published.is_some());
        assert_eq!(
            hits[1].title.as_ref(),
            "second.example",
            "a hit without a title is named by its host"
        );
        for _ in 0..3 {
            let again = cache.refresh(&held, &strings).expect("still hits");
            assert!(
                Rc::ptr_eq(&first, &again),
                "a later frame draws what the first one parsed"
            );
        }
    }

    #[test]
    fn a_new_result_or_locale_is_parsed_again() {
        let english = Strings::load("en");
        let mut cache = SearchCache::default();
        let held = FOUND.to_owned();
        let first = cache.refresh(&held, &english).expect("hits");

        // The same text held elsewhere is another result.
        let reread = held.clone();
        let second = cache.refresh(&reread, &english).expect("hits");
        assert!(!Rc::ptr_eq(&first, &second));
        assert_eq!(first, second, "the same text draws the same hits");

        let german = Strings::load("de");
        let third = cache.refresh(&reread, &german).expect("hits");
        assert!(
            !Rc::ptr_eq(&second, &third),
            "dates are written again in the new locale"
        );

        assert_eq!(
            cache.refresh("Search failed: rate limited", &german),
            None,
            "a result in another shape is text, and replaces the hits"
        );
        assert_eq!(
            cache.refresh("  No search results found.", &german),
            Some(Rc::new(SearchView::Empty))
        );
    }
}
