//! The archive page: archived sessions with Restore and Delete.
//!
//! A port of `ArchiveView.tsx`'s page layout that replaces the chat area
//! while it is open. A panel at the start lists every archived session and
//! one bucket per registered project, then the rest; the list beside it
//! shows the chosen bucket, most recently archived first, with each row's
//! archive date. The search spans every archived session, whatever bucket
//! is chosen, and an `ses_` query finds one session by its ID. A subsession
//! under an archived parent comes back with that parent and is not listed;
//! one archived alone sits under its parent's title.
//!
//! Each opening starts fresh: no query, every session, the first hundred
//! rows. What the page lists (the admitted sessions, their buckets and the
//! rows of the current query and bucket) is built once and kept until the
//! session index, the topology, the query or the bucket changes, so a frame
//! drawn for any other reason resolves no session's project again. Opening an archived session from its row, the row's context menu,
//! deleting a whole bucket, archived worktrees and the chats of deleted
//! isolated spaces are not ported. A session whose folder is gone is placed
//! by its directory alone, so one from a removed worktree lists under Other.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::{Datelike, Local, TimeZone};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, Subscription,
    TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::archive_page::{self, BucketKind, PAGE_SIZE};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::session_actions;
use openchamber_native_core::session_index::IndexStatus;
use openchamber_native_core::source_board::relative_age;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const MICRO_SIZE: f32 = 0.75;
const MICRO_LINE: f32 = 1.0;
const PAGE_WIDTH: f32 = 768.0;
const BUCKET_PANEL_WIDTH: f32 = 256.0;

/// What the open page holds between frames.
#[derive(Default)]
pub(crate) struct ArchivePage {
    /// The search field; absent until the toolbar opened the page.
    search: Option<(Entity<InputState>, Subscription)>,
    query: String,
    /// The chosen bucket's key; `None` lists every session.
    bucket: Option<String>,
    /// How many times Show more was pressed.
    extra_pages: usize,
    /// What the page lists for the current sessions, topology, query and
    /// bucket; `None` until a frame needs it and after any of them changed.
    listing: RefCell<Option<Rc<ArchiveListing>>>,
    /// How many listings were built, so a test can tell a reused one.
    #[cfg(test)]
    built: std::cell::Cell<usize>,
}

/// The page's buckets and rows, owned so they outlive the frame.
struct ArchiveListing {
    /// Every archived session the page admits.
    total: usize,
    buckets: Vec<ListedBucket>,
    /// The chosen bucket, when it still holds a session.
    chosen: Option<String>,
    /// The rows of the query, else of the chosen bucket, else every row.
    rows: Vec<ListedRow>,
}

struct ListedBucket {
    key: String,
    /// A project's label; `None` for the bucket of everything else.
    label: Option<String>,
    count: usize,
}

struct ListedRow {
    id: SessionId,
    /// Empty for an untitled session.
    title: String,
    subsession: bool,
    /// The parent's title when it shows above this row; empty if untitled.
    parent_above: Option<String>,
    /// The row's project, named while a search mixes projects.
    project: Option<SharedString>,
    /// When it was archived, else when it last changed.
    at: i64,
}

impl ArchivePage {
    /// Another server's buckets and matches are not this one's.
    pub(crate) fn switch_runtime(&mut self) {
        self.bucket = None;
        self.extra_pages = 0;
        self.invalidate();
    }

    /// The sessions, their projects or a label may have changed: the next
    /// frame of the page lists anew.
    pub(crate) fn invalidate(&mut self) {
        *self.listing.get_mut() = None;
    }

    fn set_query(&mut self, query: String) {
        if self.query != query {
            self.query = query;
            self.extra_pages = 0;
            self.invalidate();
        }
    }

    fn set_bucket(&mut self, bucket: Option<String>) {
        self.extra_pages = 0;
        if self.bucket != bucket {
            self.bucket = bucket;
            self.invalidate();
        }
    }

    fn visible(&self) -> usize {
        PAGE_SIZE * (self.extra_pages + 1)
    }
}

/// `formatSessionDateLabel`: how long ago today, Yesterday, else the date.
/// The date is written month, day, year in every locale.
fn archive_date(at_ms: i64, strings: &Strings) -> SharedString {
    let now = Local::now();
    let Some(moment) = Local.timestamp_millis_opt(at_ms).single() else {
        return SharedString::default();
    };
    let (today, date) = (now.date_naive(), moment.date_naive());
    if date == today {
        let age = relative_age(now.timestamp_millis(), at_ms);
        return if age.key == "common.relative.justNow" {
            strings.get(age.key)
        } else {
            strings.format(age.key, &[("count", &age.count.to_string())])
        };
    }
    if today.pred_opt() == Some(date) {
        return strings.get("common.date.yesterday");
    }
    let month = strings
        .dates()
        .months
        .get(date.month0() as usize)
        .cloned()
        .unwrap_or_default();
    format!("{month} {} {}", date.day(), date.year()).into()
}

/// When a session was put away, else when it last changed.
fn archived_at(session: &SessionRecord) -> i64 {
    session.time.archived.unwrap_or_else(|| session.recency())
}

impl Workspace {
    /// The toolbar's Archive button opens the page and closes it again.
    pub(crate) fn toggle_archive_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.archive_open = !self.archive_open;
        // Every opening starts fresh, and a closed page keeps no query.
        self.session_management.archive = ArchivePage::default();
        if self.archive_open {
            self.source_board_open = false;
            self.scheduled_tasks_open = false;
            let placeholder = locale::strings(cx).get("sessions.archivePage.searchPlaceholder");
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            let subscription = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
                if let InputEvent::Change = event {
                    let query = input.read(cx).value().to_string();
                    this.session_management.archive.set_query(query);
                    cx.notify();
                }
            });
            self.session_management.archive.search = Some((input, subscription));
        } else {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    /// Lists one bucket, or every session for `None`, from its first rows.
    fn select_archive_bucket(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        self.session_management.archive.set_bucket(key);
        cx.notify();
    }

    fn show_more_archived(&mut self, cx: &mut Context<Self>) {
        self.session_management.archive.extra_pages += 1;
        cx.notify();
    }

    /// The registered project an archived session's directory is in.
    fn archive_project_root(&self, session: &SessionRecord) -> Option<String> {
        session
            .resolved_directory()
            .and_then(|directory| self.project_of_directory(&directory))
    }

    /// What the page lists now, built on the first frame that needs it.
    fn archive_listing(&self) -> Rc<ArchiveListing> {
        let page = &self.session_management.archive;
        if let Some(listing) = page.listing.borrow().as_ref() {
            return listing.clone();
        }
        let listing = Rc::new(self.build_archive_listing());
        #[cfg(test)]
        page.built.set(page.built.get() + 1);
        *page.listing.borrow_mut() = Some(listing.clone());
        listing
    }

    fn build_archive_listing(&self) -> ArchiveListing {
        let page = &self.session_management.archive;
        let sessions: Vec<&SessionRecord> = session_actions::archive_page(&self.sessions)
            .into_iter()
            .filter(|session| self.session_admitted(&session.id))
            .collect();
        let buckets = archive_page::buckets(
            &sessions,
            |session| self.archive_project_root(session),
            |root| self.project_display_label(root).to_string(),
            "",
        );
        // A bucket that emptied since it was chosen lists every session.
        let chosen = page
            .bucket
            .as_ref()
            .and_then(|key| buckets.iter().find(|bucket| bucket.key == *key));
        let searching = !page.query.trim().is_empty();
        let listed = archive_page::rows(&sessions, &page.query, chosen);
        let mut rows = Vec::with_capacity(listed.len());
        for (index, session) in listed.iter().enumerate() {
            let session: &SessionRecord = session;
            // A subsession archived alone reads under its parent's title.
            let parent_above = archive_page::parent_above(&listed, index).map(|parent| {
                self.sessions
                    .get(parent)
                    .map(|parent| parent.title.clone())
                    .unwrap_or_default()
            });
            // A search mixes projects, so each match names its own.
            let project = searching
                .then(|| self.archive_project_root(session))
                .flatten()
                .map(|root| self.project_display_label(&root));
            rows.push(ListedRow {
                id: session.id.clone(),
                title: session.title.clone(),
                subsession: session.parent_id.is_some(),
                parent_above,
                project,
                at: archived_at(session),
            });
        }
        ArchiveListing {
            total: sessions.len(),
            chosen: chosen.map(|bucket| bucket.key.clone()),
            buckets: buckets
                .iter()
                .map(|bucket| ListedBucket {
                    key: bucket.key.clone(),
                    label: (bucket.kind == BucketKind::Project).then(|| bucket.label.clone()),
                    count: bucket.sessions.len(),
                })
                .collect(),
            rows,
        }
    }

    pub(crate) fn render_archive_page(
        &self,
        palette: &Palette,
        direction: Direction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let page = &self.session_management.archive;
        let listing = self.archive_listing();
        let total = listing.total;
        let searching = !page.query.trim().is_empty();
        let count = listing.rows.len();
        let count_label = if count == 1 {
            strings.format(
                "sessions.archivePage.countSingle",
                &[("count", &count.to_string())],
            )
        } else {
            strings.format(
                "sessions.archivePage.countPlural",
                &[("count", &count.to_string())],
            )
        };
        // Empty means no archived sessions only once the list has loaded.
        let empty_label = match self.sessions.status() {
            IndexStatus::Failed if total == 0 && !self.sessions.has_loaded() => {
                strings.get("sessions.sidebar.group.empty.loadFailed")
            }
            _ if total == 0 && !self.sessions.has_loaded() && !self.is_synthetic() => {
                strings.get("sessions.sidebar.group.empty.loadingSessions")
            }
            _ if searching => strings.get("sessions.archivePage.empty.noMatches"),
            _ => strings.get("sessions.archivePage.empty.noArchived"),
        };
        let untitled = strings.get("sessions.sidebar.session.untitled");
        let title_of = |title: &str| -> SharedString {
            if title.is_empty() {
                untitled.clone()
            } else {
                title.to_owned().into()
            }
        };
        let visible = &listing.rows[..count.min(page.visible())];
        let mut rows: Vec<AnyElement> = Vec::with_capacity(visible.len());
        for session in visible {
            let id = session.id.clone();
            let title = title_of(session.title.as_str());
            if let Some(parent_title) = &session.parent_above {
                rows.push(
                    div()
                        .pt(px(8.0))
                        .padding_start(direction, px(8.0))
                        .padding_end(direction, px(8.0))
                        .truncate()
                        .css_text(MICRO_SIZE, MICRO_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(title_of(parent_title.as_str()))
                        .into_any_element(),
                );
            }
            let row_key = format!("archive-row:{id}");
            let group = SharedString::from(format!("{row_key}:hover"));
            let action =
                |name: &str, icon: &'static str, label: SharedString, hover: gpui_kit::Hsla| {
                    let key = format!("archive-{name}:{id}");
                    div()
                        .id(SharedString::from(key.clone()))
                        .group(SharedString::from(key.clone()))
                        .role(Role::Button)
                        .accessibility_id(key.clone())
                        .aria_label(label)
                        .size(px(24.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .opacity(0.0)
                        .group_hover(group.clone(), |style| style.opacity(1.0))
                        .focus(|style| style.opacity(1.0))
                        .child(
                            glyph(icon, 14.0, palette.muted_foreground)
                                .group_hover(SharedString::from(key), move |style| {
                                    style.text_color(hover)
                                }),
                        )
                };
            let restore_id = id.clone();
            let delete_id = id.clone();
            let restore = on_activate(
                action(
                    "restore",
                    "inbox-unarchive",
                    strings.format(
                        "sessions.archivePage.restoreSessionAria",
                        &[("title", &title)],
                    ),
                    palette.foreground,
                ),
                cx,
                move |this, window, cx| {
                    this.restore_sessions(vec![restore_id.clone()], None, false, window, cx);
                },
            )
            .test_support();
            let delete = on_activate(
                action(
                    "delete",
                    "delete-bin",
                    strings.format(
                        "sessions.archivePage.deleteSessionAria",
                        &[("title", &title)],
                    ),
                    palette.error,
                ),
                cx,
                move |this, window, cx| this.request_removal(&delete_id, true, window, cx),
            )
            .test_support();
            let project = session.project.clone();
            let date = archive_date(session.at, &strings);
            let meta = |text: SharedString| {
                div()
                    .flex_shrink_0()
                    .max_w(px(160.0))
                    .truncate()
                    .css_text(MICRO_SIZE, MICRO_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(text)
            };
            rows.push(
                div()
                    .id(SharedString::from(row_key.clone()))
                    .role(Role::ListItem)
                    .accessibility_id(row_key)
                    .aria_label(title.clone())
                    .aria_description(date.clone())
                    .group(group)
                    .h(px(28.0))
                    .padding_start(direction, px(if session.subsession { 24.0 } else { 8.0 }))
                    .padding_end(direction, px(4.0))
                    .rounded(px(6.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .hover(|style| style.bg(palette.hover.opacity(0.4)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(LABEL_SIZE, LABEL_LINE, cx)
                            .text_color(palette.foreground)
                            .child(title),
                    )
                    .children(project.map(&meta))
                    .child(meta(date))
                    .child(restore)
                    .child(delete)
                    .test_support()
                    .into_any_element(),
            );
        }
        let remaining = count - visible.len();
        let show_more = (remaining > 0).then(|| {
            let label = strings.get("sessions.sidebar.group.showMore");
            let foreground = palette.foreground;
            on_activate(
                div()
                    .id("archive-show-more")
                    .role(Role::Button)
                    .accessibility_id("archive-show-more")
                    .aria_label(label.clone())
                    .mt(px(4.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .flex_row_from(direction)
                    .cursor_pointer()
                    .css_text(MICRO_SIZE, MICRO_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .hover(move |style| style.text_color(foreground).underline())
                    .child(label),
                cx,
                |this, _, cx| this.show_more_archived(cx),
            )
            .test_support()
        });
        let search = page.search.as_ref().map(|(input, _)| {
            div()
                .id("archive-search")
                .role(Role::Search)
                .accessibility_id("archive-search")
                .flex_1()
                .min_w_0()
                .h(px(32.0))
                .px(px(8.0))
                .rounded(px(8.0))
                .bg(palette.background)
                .border_1()
                .border_color(palette.border)
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .child(glyph("search", 14.0, palette.muted_foreground))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(input)
                            .appearance(false)
                            .bordered(false)
                            .text_size(gpui_kit::rems(LABEL_SIZE)),
                    ),
                )
                .test_support()
        });
        let panel = self.render_archive_buckets(&listing, palette, direction, cx);
        div()
            .id("archive-page")
            .role(Role::Region)
            .accessibility_id("archive-page")
            .aria_label(strings.get("sessions.archivePage.title"))
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(palette.background)
            .flex_row_from(direction)
            .child(panel)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div().flex_shrink_0().px(px(24.0)).pt(px(12.0)).child(
                            div()
                                .mx_auto()
                                .w_full()
                                .max_w(px(PAGE_WIDTH))
                                .h(px(32.0))
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(12.0))
                                .children(search)
                                .when(page.search.is_none(), |this| this.child(div().flex_1()))
                                .child(
                                    div()
                                        .id("archive-count")
                                        .role(Role::Status)
                                        .accessibility_id("archive-count")
                                        .aria_label(count_label.clone())
                                        .flex_shrink_0()
                                        .css_text(MICRO_SIZE, MICRO_LINE, cx)
                                        .text_color(palette.muted_foreground)
                                        .child(count_label),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .id("archive-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(px(24.0))
                            .py(px(12.0))
                            .child(
                                div()
                                    .mx_auto()
                                    .w_full()
                                    .max_w(px(PAGE_WIDTH))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .when(rows.is_empty(), |this| {
                                        this.child(
                                            div()
                                                .id("archive-empty")
                                                .role(Role::Status)
                                                .accessibility_id("archive-empty")
                                                .aria_label(empty_label.clone())
                                                .py(px(40.0))
                                                .text_center()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                                                .text_color(palette.muted_foreground)
                                                .child(empty_label),
                                        )
                                    })
                                    .children(rows)
                                    .children(show_more),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The filter panel: every session, then each bucket with its count.
    fn render_archive_buckets(
        &self,
        listing: &ArchiveListing,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let hover_fill = palette.hover.opacity(0.5);
        let foreground = palette.foreground;
        let mut entries: Vec<(Option<String>, SharedString, usize)> =
            vec![(None, strings.get("sessions.archivePage.all"), listing.total)];
        for bucket in &listing.buckets {
            let label = match &bucket.label {
                Some(label) => label.clone().into(),
                None => strings.get("sessions.archivePage.other"),
            };
            entries.push((Some(bucket.key.clone()), label, bucket.count));
        }
        let chosen_key = listing.chosen.as_deref();
        let mut items: Vec<AnyElement> = Vec::with_capacity(entries.len());
        for (index, (key, label, count)) in entries.into_iter().enumerate() {
            let selected = key.as_deref() == chosen_key;
            let id = format!("archive-bucket:{index}");
            let item = div()
                .id(SharedString::from(id.clone()))
                .role(Role::Button)
                .accessibility_id(id)
                .aria_label(label.clone())
                .aria_selected(selected)
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .when(selected, |this| {
                    this.bg(palette.selection).text_color(palette.foreground)
                })
                .when(!selected, |this| {
                    this.text_color(palette.muted_foreground)
                        .hover(move |style| style.bg(hover_fill).text_color(foreground))
                })
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(
                    div()
                        .flex_shrink_0()
                        .css_text(MICRO_SIZE, MICRO_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(SharedString::from(count.to_string())),
                );
            items.push(
                on_activate(item, cx, move |this, _, cx| {
                    this.select_archive_bucket(key.clone(), cx)
                })
                .test_support()
                .into_any_element(),
            );
        }
        div()
            .relative()
            .w(px(BUCKET_PANEL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .child(
                div()
                    .id("archive-buckets")
                    .role(Role::Group)
                    .accessibility_id("archive-buckets")
                    .aria_label(strings.get("sessions.archivePage.title"))
                    .size_full()
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(items)
                    .test_support(),
            )
            // The divider toward the list: the panel and the list are both
            // canvases of one tone.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .inset_end(direction, px(0.0))
                    .w(px(1.0))
                    .bg(palette.border),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, ElementId, SharedString, TestAppContext};
    use openchamber_native_core::archive_page;
    use openchamber_native_core::ids::SessionId;
    use openchamber_native_core::session::{SessionPatch, SessionRecord};
    use openchamber_native_core::session_actions;
    use openchamber_native_transport::event::ServerEvent;

    use super::{ArchivePage, archive_date, archived_at};
    use crate::sidebar_item::SidebarItem;
    use crate::workspace_tests::open;

    fn name(id: impl Into<String>) -> ElementId {
        ElementId::Name(SharedString::from(id.into()))
    }

    #[test]
    fn show_more_reveals_a_page_at_a_time_and_a_runtime_switch_starts_over() {
        let mut page = ArchivePage::default();
        assert_eq!(page.visible(), archive_page::PAGE_SIZE);
        page.extra_pages = 2;
        page.bucket = Some("/work/a".to_owned());
        assert_eq!(page.visible(), 3 * archive_page::PAGE_SIZE);
        page.switch_runtime();
        assert_eq!(page.visible(), archive_page::PAGE_SIZE);
        assert_eq!(page.bucket, None);
    }

    #[test]
    fn a_row_dates_by_its_archive_time_then_its_last_change() {
        let mut session = SessionRecord::new("a", "/p");
        session.time.created = 5;
        assert_eq!(archived_at(&session), 5);
        session.time.updated = 9;
        assert_eq!(archived_at(&session), 9);
        session.time.archived = Some(30);
        assert_eq!(archived_at(&session), 30);
    }

    #[gpui_kit::test]
    fn dates_read_relative_today_and_absolute_before(cx: &mut TestAppContext) {
        let (_, _workspace) = open(cx);
        let strings = cx.read(|cx| crate::locale::strings(cx).clone());
        let now = chrono::Local::now().timestamp_millis();
        // Two days back is neither today nor yesterday in any time zone.
        let old = now - 2 * 86_400_000 - 3_600_000;
        let label = archive_date(old, &strings).to_string();
        let year = chrono::TimeZone::timestamp_millis_opt(&chrono::Local, old)
            .single()
            .map(|moment| chrono::Datelike::year(&moment).to_string())
            .expect("a local time");
        assert!(label.ends_with(&year), "{label} ends with the year");
        assert_ne!(label, strings.get("common.date.yesterday").to_string());
    }

    #[gpui_kit::test]
    fn the_listing_is_built_once_until_its_inputs_change(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let built = |cx: &mut TestAppContext| {
            cx.read(|cx| workspace.read(cx).session_management.archive.built.get())
        };
        // Runs `change` on the workspace, draws a frame and says how many
        // listings that built.
        let frame = |cx: &mut TestAppContext,
                     change: &dyn Fn(
            &mut crate::workspace::Workspace,
            &mut gpui_kit::Window,
            &mut gpui_kit::Context<crate::workspace::Workspace>,
        )| {
            let before = built(cx);
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |state, cx| change(state, window, cx));
                window.render_frame(cx);
            })
            .expect("window is open");
            built(cx) - before
        };
        assert_eq!(
            frame(cx, &|state, window, cx| state
                .toggle_archive_page(window, cx)),
            1,
            "the first frame of the page lists"
        );
        for _ in 0..3 {
            assert_eq!(
                frame(cx, &|_, _, cx| cx.notify()),
                0,
                "a frame drawn for another reason reuses the listing"
            );
        }
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(
                std::rc::Rc::ptr_eq(&state.archive_listing(), &state.archive_listing()),
                "two reads share one listing"
            );
        });
        assert_eq!(frame(cx, &|_, _, cx| cx.notify()), 0);
        let query = |state: &mut crate::workspace::Workspace, text: &str| {
            state.session_management.archive.set_query(text.to_owned());
        };
        assert_eq!(
            frame(cx, &|state, _, cx| {
                query(state, "relay");
                cx.notify();
            }),
            1,
            "a new query lists anew"
        );
        assert_eq!(
            frame(cx, &|state, _, cx| {
                query(state, "relay");
                cx.notify();
            }),
            0,
            "the same query does not"
        );
        assert_eq!(
            frame(cx, &|state, _, cx| {
                state.select_archive_bucket(Some("/no/such/project".to_owned()), cx)
            }),
            1,
            "a new bucket lists anew"
        );
        assert_eq!(
            frame(cx, &|state, _, cx| state.show_more_archived(cx)),
            0,
            "Show more reveals rows of the same listing"
        );
        // A session is archived elsewhere: the index changed.
        let session = cx.read(|cx| {
            workspace
                .read(cx)
                .sessions
                .active()
                .first()
                .map(|session| session.id.clone())
                .expect("the fixture has sessions")
        });
        assert_eq!(
            frame(cx, &|state, window, cx| {
                state.apply_server_events(
                    vec![ServerEvent::SessionPatched {
                        session_id: session.clone(),
                        patch: SessionPatch {
                            archived: Some(Some(11)),
                            ..SessionPatch::default()
                        },
                    }],
                    window,
                    cx,
                );
            }),
            1,
            "an index change lists anew"
        );
        // Projects, worktrees and labels reach the page through the sidebar
        // rebuild they already cause.
        assert_eq!(
            frame(cx, &|state, _, cx| {
                state.rebuild_sidebar();
                cx.notify();
            }),
            1,
            "a topology change lists anew"
        );
    }

    #[gpui_kit::test]
    fn the_page_lists_buckets_and_a_bucket_narrows_the_list(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        // Archive the first two listed sessions that belong to a project.
        let targets: Vec<SessionId> = cx.read(|cx| {
            let state = workspace.read(cx);
            let mut targets: Vec<SessionId> = Vec::new();
            for item in &state.sidebar_items {
                let SidebarItem::Session {
                    session_id,
                    depth: 0,
                    ..
                } = item
                else {
                    continue;
                };
                let owned = state
                    .sessions
                    .get(session_id)
                    .is_some_and(|session| state.archive_project_root(session).is_some());
                if owned && state.session_admitted(session_id) && !targets.contains(session_id) {
                    targets.push(session_id.clone());
                }
                if targets.len() == 2 {
                    break;
                }
            }
            targets
        });
        assert_eq!(targets.len(), 2);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |state, cx| {
                let events = targets
                    .iter()
                    .enumerate()
                    .map(|(index, id)| ServerEvent::SessionPatched {
                        session_id: id.clone(),
                        patch: SessionPatch {
                            archived: Some(Some(10 + index as i64)),
                            ..SessionPatch::default()
                        },
                    })
                    .collect();
                state.apply_server_events(events, window, cx);
                state.toggle_archive_page(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find(name("archive-search")).is_some());
            assert!(window.try_find(name("archive-bucket:0")).is_some());
        })
        .expect("window is open");
        // What the page lists, and how the core buckets it.
        let (listed, first_bucket) = cx.read(|cx| {
            let state = workspace.read(cx);
            let sessions: Vec<&SessionRecord> = session_actions::archive_page(&state.sessions)
                .into_iter()
                .filter(|session| state.session_admitted(&session.id))
                .collect();
            let buckets = archive_page::buckets(
                &sessions,
                |session| state.archive_project_root(session),
                |root| state.project_display_label(root).to_string(),
                "Other",
            );
            let first: Vec<SessionId> = buckets
                .first()
                .map(|bucket| bucket.sessions.iter().map(|row| row.id.clone()).collect())
                .unwrap_or_default();
            (
                sessions
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>(),
                first,
            )
        });
        assert!(targets.iter().all(|id| listed.contains(id)));
        assert!(!first_bucket.is_empty(), "the first bucket holds sessions");
        cx.update_window(window, |_, window, cx| {
            for id in &listed {
                assert!(
                    window.try_find(name(format!("archive-row:{id}"))).is_some(),
                    "every archived session lists at first"
                );
            }
            window.click(name("archive-bucket:1"), cx);
            window.render_frame(cx);
            for id in &listed {
                assert_eq!(
                    window.try_find(name(format!("archive-row:{id}"))).is_some(),
                    first_bucket.contains(id),
                    "the chosen bucket lists only its own sessions"
                );
            }
            // Closing forgets the bucket.
            workspace.update(cx, |state, cx| {
                state.toggle_archive_page(window, cx);
                state.toggle_archive_page(window, cx);
            });
            window.render_frame(cx);
            for id in &listed {
                assert!(window.try_find(name(format!("archive-row:{id}"))).is_some());
            }
        })
        .expect("window is open");
    }
}
