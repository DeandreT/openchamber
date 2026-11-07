//! Draws the source board: the toolbar (kind tabs, project, search, filter,
//! refresh), the status line, the list with its loading, failure, empty and
//! not-connected states, and the preview of the highlighted item with its
//! state menu, details, comments and reply. Every control dispatches to
//! `SourceBoard`; nothing here decides what the server said.

use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::{
    AnyElement, ClipboardItem, Context, EntityInputHandler, FontWeight, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, prelude::FluentBuilder, px,
    rems,
};
use openchamber_native_core::source_board::{
    ChecksState, ChecksSummary, Comment, CommentReview, ListStatus, ProviderTerms, Reference,
    RepositoryKind, ReviewVerdict, StateTone, Unavailable, relative_age, state_look,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::elevation;
use crate::files_view::{status_line, text_button};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::markdown_view::{self, Author};
use crate::source_board::{Busy, DetailLoad, Menu, ProjectSource, SourceBoard};
use crate::theme::Palette;

const HEADER_SIZE: f32 = 0.875;
const HEADER_LINE: f32 = 1.25;
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const META_SIZE: f32 = 0.781_25;
const META_LINE: f32 = 1.125;
const MICRO_SIZE: f32 = 0.75;
const MICRO_LINE: f32 = 1.0625;
const LIST_MIN_WIDTH: f32 = 280.0;

type Action = Box<dyn Fn(&mut SourceBoard, &mut Window, &mut Context<SourceBoard>)>;
/// `ReferencePickerRow` shows at most two labels on a row.
const ROW_LABELS: usize = 2;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// "3h ago" for an ISO time; none when it does not parse.
fn age_label(strings: &Strings, iso: Option<&str>) -> Option<SharedString> {
    let at = chrono::DateTime::parse_from_rfc3339(iso?).ok()?;
    let age = relative_age(now_ms(), at.timestamp_millis());
    Some(if age.key == "common.relative.justNow" {
        strings.get(age.key)
    } else {
        strings.format(age.key, &[("count", &age.count.to_string())])
    })
}

fn tone_color(tone: StateTone, palette: &Palette) -> Hsla {
    match tone {
        StateTone::Open => palette.pr_open,
        StateTone::Draft => palette.pr_draft,
        StateTone::Blocked => palette.pr_blocked,
        StateTone::Merged => palette.pr_merged,
        StateTone::Closed => palette.pr_closed,
    }
}

fn label_color(hex: Option<&str>, palette: &Palette) -> Hsla {
    hex.and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .map(|rgb| gpui_kit::rgb(rgb).into())
        .unwrap_or(palette.muted_foreground)
}

/// The keys whose wording follows the provider's name for a change
/// request: pull request, merge request or change.
struct TermKeys {
    tab: &'static str,
    search: &'static str,
    not_connected: &'static str,
    empty: &'static str,
    list: &'static str,
    close: &'static str,
    reopen: &'static str,
}

fn term_keys(term: &str) -> TermKeys {
    match term {
        "mergeRequest" => TermKeys {
            tab: "references.picker.tab.mergeRequests",
            search: "references.picker.search.mergeRequest",
            not_connected: "references.picker.empty.notConnected.mergeRequest",
            empty: "references.picker.empty.mergeRequests",
            list: "sourceBoard.list.repository.mergeRequest",
            close: "sourceBoard.actions.closePull.mergeRequest",
            reopen: "sourceBoard.actions.reopenPull.mergeRequest",
        },
        "change" => TermKeys {
            tab: "references.picker.tab.changes",
            search: "references.picker.search.change",
            not_connected: "references.picker.empty.notConnected.change",
            empty: "references.picker.empty.changes",
            list: "sourceBoard.list.repository.change",
            close: "sourceBoard.actions.closePull.change",
            reopen: "sourceBoard.actions.reopenPull.change",
        },
        _ => TermKeys {
            tab: "references.picker.tab.pulls",
            search: "references.picker.search.pullRequest",
            not_connected: "references.picker.empty.notConnected.pullRequest",
            empty: "references.picker.empty.pulls",
            list: "sourceBoard.list.repository.pullRequest",
            close: "sourceBoard.actions.closePull.pullRequest",
            reopen: "sourceBoard.actions.reopenPull.pullRequest",
        },
    }
}

impl SourceBoard {
    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.search.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    /// Arrows and Ctrl+N/P from the search field move the highlight.
    fn search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return;
        }
        let key = &event.keystroke;
        let plain = !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform;
        let control = key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform;
        let forward = match key.key.as_str() {
            "down" if plain => true,
            "n" if control => true,
            "up" if plain => false,
            "p" if control => false,
            _ => return,
        };
        self.move_highlight(forward, cx);
        cx.stop_propagation();
    }

    /// Reads the next page once the list scrolled within three rows of its end.
    fn maybe_load_more(&mut self, cx: &mut Context<Self>) {
        if self.list.has_more()
            && !self.list.is_loading_more()
            && self.list_scroll.bottom_item() + 3 >= self.list.items().len()
        {
            self.load_more(cx);
        }
    }

    fn render_toolbar(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let term = self
            .context()
            .map(|(_, terms)| terms.change_request.clone())
            .unwrap_or_else(|| "pullRequest".to_owned());
        let tab = |kind: RepositoryKind, icon: &'static str, label: SharedString| {
            let selected = self.kind == kind;
            let id = format!("source-board-kind:{}", kind.as_str());
            let hover = palette.hover;
            on_activate(
                div()
                    .id(SharedString::from(id.clone()))
                    .role(Role::Tab)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .h(px(24.0))
                    .px(px(10.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(palette.background)
                            .shadow(elevation::control(palette))
                    })
                    .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                    .child(glyph(
                        icon,
                        14.0,
                        if selected {
                            palette.foreground
                        } else {
                            palette.muted_foreground
                        },
                    ))
                    .child(
                        div()
                            .css_text(MICRO_SIZE, MICRO_LINE, cx)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if selected {
                                palette.foreground
                            } else {
                                palette.muted_foreground
                            })
                            .child(label),
                    ),
                cx,
                move |this: &mut Self, _, cx| this.select_kind(kind, cx),
            )
            .test_support()
        };
        let tabs = div()
            .id("source-board-kinds")
            .role(Role::TabList)
            .accessibility_id("source-board-kinds")
            .p(px(2.0))
            .rounded(px(CONTROL_RADIUS + 2.0))
            .bg(palette.muted)
            .flex_row_from(direction)
            .gap(px(2.0))
            .flex_shrink_0()
            .child(tab(
                RepositoryKind::Issue,
                "record-circle",
                strings.get("references.picker.tab.issues"),
            ))
            .child(tab(
                RepositoryKind::Pull,
                "git-pull-request",
                strings.get(term_keys(&term).tab),
            ));
        let project_label = self
            .project
            .as_ref()
            .and_then(|path| self.projects.iter().find(|project| &project.path == path))
            .map(|project| project.label.clone());
        let project = project_label.map(|label| {
            self.dropdown_trigger(
                "source-board-project",
                "folder",
                label,
                strings.get("sourceBoard.project.label"),
                Menu::Project,
                palette,
                direction,
                cx,
            )
        });
        let filter = self.filter();
        let mut summary = strings.get(filter.state.label_key()).to_string();
        if filter.people != Default::default() {
            summary.push_str(" · ");
            summary.push_str(&strings.get(filter.people.label_key()));
        }
        let placeholder = strings.get(term_keys(&term).search);
        let has_text = !self.search.read(cx).value().is_empty();
        let search = div()
            .id("source-board-search-box")
            .capture_key_down(cx.listener(Self::search_key))
            .flex_1()
            .min_w(px(224.0))
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(CONTROL_RADIUS))
            .bg(palette.background)
            .shadow(elevation::control(palette))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .child(glyph("search", 14.0, palette.muted_foreground))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .appearance(false)
                        .bordered(false)
                        .text_size(rems(LABEL_SIZE))
                        .accessibility_id("source-board-search")
                        .aria_label(placeholder),
                ),
            )
            .when(self.list.is_refreshing(), |this| {
                this.child(glyph("loader-4", 14.0, palette.muted_foreground))
            })
            .when(has_text, |this| {
                this.child(
                    IconButton::new(
                        "source-board-search-clear",
                        "close",
                        strings.get("references.picker.actions.clearSearch"),
                    )
                    .size(20.0, 14.0)
                    .render(palette, cx, |this: &mut Self, window, cx| {
                        this.clear_search(window, cx)
                    }),
                )
            });
        div()
            .id("source-board-toolbar")
            .relative()
            .flex_shrink_0()
            .min_h(px(48.0))
            .px(px(8.0))
            .py(px(10.0))
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(palette.divider)
            .child(tabs)
            .children(project)
            .child(search)
            .child(self.dropdown_trigger(
                "source-board-filter",
                "filter-3",
                summary.into(),
                strings.get("references.picker.filter.label"),
                Menu::Filter,
                palette,
                direction,
                cx,
            ))
            .child(
                IconButton::new(
                    "source-board-refresh",
                    "refresh",
                    strings.get("sourceBoard.list.refresh"),
                )
                .size(28.0, 16.0)
                .tone(crate::chrome::ButtonTone::QuietFilled)
                .render(palette, cx, |this: &mut Self, _, cx| this.refresh_list(cx)),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn dropdown_trigger(
        &self,
        id: &'static str,
        icon: &'static str,
        value: SharedString,
        label: SharedString,
        menu: Menu,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.menu == Some(menu);
        let hover = palette.hover;
        on_activate(
            div()
                .id(id)
                .role(Role::Button)
                .accessibility_id(id)
                .aria_label(SharedString::from(format!("{label}: {value}")))
                .aria_expanded(open)
                .h(px(28.0))
                .max_w(px(288.0))
                .px(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_shrink_0()
                .bg(palette.background)
                .shadow(elevation::control(palette))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(glyph(icon, 14.0, palette.muted_foreground))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .text_color(palette.foreground)
                        .child(value),
                )
                .child(glyph("arrow-down-s", 14.0, palette.muted_foreground)),
            cx,
            move |this: &mut Self, _, cx| this.toggle_menu(menu, cx),
        )
        .test_support()
        .into_any_element()
    }

    /// One option of a floating menu.
    #[allow(clippy::too_many_arguments)]
    fn menu_option(
        &self,
        id: String,
        label: SharedString,
        checked: bool,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let hover = palette.hover;
        on_activate(
            div()
                .id(SharedString::from(id.clone()))
                .role(Role::MenuItemRadio)
                .accessibility_id(id)
                .aria_label(label.clone())
                .aria_selected(checked)
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .text_color(palette.foreground)
                        .child(label),
                )
                .when(checked, |this| {
                    this.child(glyph("check", 14.0, palette.foreground))
                }),
            cx,
            action,
        )
        .test_support()
        .into_any_element()
    }

    fn menu_caption(
        &self,
        text: SharedString,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .px(px(8.0))
            .pt(px(6.0))
            .pb(px(2.0))
            .css_text(MICRO_SIZE, MICRO_LINE, cx)
            .text_color(palette.muted_foreground)
            .child(text)
            .into_any_element()
    }

    fn render_menu(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.menu.filter(|menu| *menu != Menu::State)?;
        let strings = locale::strings(cx).clone();
        let mut items: Vec<AnyElement> = Vec::new();
        match menu {
            Menu::Project => {
                for project in &self.projects {
                    let path = project.path.clone();
                    items.push(self.menu_option(
                        format!("source-board-project-option:{}", project.path),
                        project.label.clone(),
                        self.project.as_deref() == Some(project.path.as_str()),
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, _, cx| this.select_project(Some(path.clone()), cx),
                    ));
                }
            }
            Menu::Filter => {
                let filter = self.filter();
                items.push(self.menu_caption(
                    strings.get("references.picker.filter.group.status"),
                    palette,
                    cx,
                ));
                for state in self.kind.states() {
                    let state = *state;
                    items.push(self.menu_option(
                        format!("source-board-filter-state:{}", state.as_str()),
                        strings.get(state.label_key()),
                        filter.state == state,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, _, cx| this.select_state(state, cx),
                    ));
                }
                items.push(
                    div()
                        .h(px(1.0))
                        .my(px(4.0))
                        .bg(palette.divider)
                        .into_any_element(),
                );
                items.push(self.menu_caption(
                    strings.get("references.picker.filter.group.people"),
                    palette,
                    cx,
                ));
                for people in self.kind.people() {
                    let people = *people;
                    items.push(self.menu_option(
                        format!("source-board-filter-people:{}", people.as_str()),
                        strings.get(people.label_key()),
                        filter.people == people,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, _, cx| this.select_people(people, cx),
                    ));
                }
            }
            Menu::State => {}
        }
        Some(
            div()
                .id("source-board-menu")
                .role(Role::Menu)
                .accessibility_id("source-board-menu")
                .absolute()
                .top(px(44.0))
                .inset_end(direction, px(44.0))
                .w(px(240.0))
                .max_h(px(320.0))
                .overflow_y_scroll()
                .p(px(4.0))
                .rounded(px(10.0))
                .bg(palette.elevated)
                .shadow(elevation::floating(palette))
                .occlude()
                .children(items)
                .into_any_element(),
        )
    }

    /// The whole board when the project offers no list: loading, nothing to
    /// read, a sign-in to make, or a failure with a way to try again.
    fn render_source_state(&self, palette: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let strings = locale::strings(cx).clone();
        let text: SharedString = match &self.source {
            ProjectSource::Ready(_) => return None,
            ProjectSource::Resolving => strings.get("references.picker.loading"),
            ProjectSource::NoProject => strings.get("sourceBoard.empty.noProjects"),
            ProjectSource::NoSource => strings.get("sourceBoard.empty.noSource"),
            ProjectSource::Unsupported => strings.get("references.picker.empty.unsupported"),
            ProjectSource::Unauthenticated { provider, term } => {
                strings.format(term_keys(term).not_connected, &[("provider", provider)])
            }
            ProjectSource::Failed(error) => {
                strings.format("references.picker.error.load", &[("error", error)])
            }
        };
        let failed = matches!(self.source, ProjectSource::Failed(_));
        Some(self.centered(
            "source-board-state",
            text,
            failed,
            failed.then(|| {
                text_button(
                    "source-board-retry",
                    strings.get("references.picker.actions.retry"),
                    None,
                    palette,
                    cx,
                    |this: &mut Self, _, cx| this.resolve(cx),
                )
            }),
            palette,
            cx,
        ))
    }

    fn centered(
        &self,
        id: &'static str,
        text: SharedString,
        error: bool,
        action: Option<AnyElement>,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .size_full()
            .min_h(px(192.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .px(px(24.0))
            .child(
                div()
                    .id(id)
                    .role(Role::Status)
                    .accessibility_id(id)
                    .aria_label(text.clone())
                    .test_support()
                    .text_center()
                    .css_text(META_SIZE, META_LINE, cx)
                    .text_color(if error {
                        palette.status_error
                    } else {
                        palette.muted_foreground
                    })
                    .child(text),
            )
            .children(action)
            .into_any_element()
    }

    fn render_list(
        &self,
        terms: &ProviderTerms,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let term = terms.change_request.as_str();
        let retry = |cx: &mut Context<Self>| {
            Some(text_button(
                "source-board-list-retry",
                strings.get("references.picker.actions.retry"),
                None,
                palette,
                cx,
                |this: &mut Self, _, cx| this.load_list(cx),
            ))
        };
        match self.list.status() {
            ListStatus::Loading => {
                return self.centered(
                    "source-board-list-state",
                    strings.get("references.picker.loading"),
                    false,
                    None,
                    palette,
                    cx,
                );
            }
            ListStatus::Unavailable(Unavailable::Disconnected) => {
                return self.centered(
                    "source-board-list-state",
                    strings.format(term_keys(term).not_connected, &[("provider", &terms.label)]),
                    false,
                    None,
                    palette,
                    cx,
                );
            }
            ListStatus::Unavailable(Unavailable::NoRepository) => {
                return self.centered(
                    "source-board-list-state",
                    strings.get("references.picker.empty.noRepo"),
                    false,
                    None,
                    palette,
                    cx,
                );
            }
            ListStatus::Failed(error) => {
                let action = retry(cx);
                return self.centered(
                    "source-board-list-state",
                    strings.format("references.picker.error.load", &[("error", error)]),
                    true,
                    action,
                    palette,
                    cx,
                );
            }
            ListStatus::Ready => {}
        }
        let items = self.list.items();
        let highlighted = self.list.highlighted_key();
        let banner = self.list.refresh_error().map(|error| {
            let text = strings.format("references.picker.error.refresh", &[("error", error)]);
            div()
                .id("source-board-refresh-error")
                .role(Role::Status)
                .accessibility_id("source-board-refresh-error")
                .aria_label(text.clone())
                .m(px(6.0))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(8.0))
                .bg(palette.status_error.opacity(0.12))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .css_text(META_SIZE, META_LINE, cx)
                        .text_color(palette.status_error)
                        .child(text),
                )
                .children(retry(cx))
        });
        let empty = items.is_empty().then(|| {
            let key = if !self.search_text().trim().is_empty() {
                "references.picker.empty.search".to_owned()
            } else if self.kind == RepositoryKind::Issue {
                "references.picker.empty.issues".to_owned()
            } else {
                term_keys(term).empty.to_owned()
            };
            self.centered(
                "source-board-list-state",
                strings.get(&key),
                false,
                None,
                palette,
                cx,
            )
        });
        let rows: Vec<AnyElement> = items
            .iter()
            .map(|item| {
                let key = item.key();
                self.render_row(
                    item,
                    terms,
                    highlighted.as_deref() == Some(key.as_str()),
                    palette,
                    direction,
                    cx,
                )
            })
            .collect();
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(banner)
            .children(empty)
            .child(
                div()
                    .id("source-board-list")
                    .role(Role::ListBox)
                    .accessibility_id("source-board-list")
                    .aria_label(strings.format(term_keys(term).list, &[("provider", &terms.label)]))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .on_scroll_wheel(cx.listener(|this, _, _, cx| this.maybe_load_more(cx)))
                    .p(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .children(rows)
                    .when(self.list.has_more(), |this| {
                        this.child(
                            div()
                                .id("source-board-more")
                                .h(px(32.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(self.list.is_loading_more(), |this| {
                                    this.child(glyph("loader-4", 16.0, palette.muted_foreground))
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_row(
        &self,
        item: &Reference,
        terms: &ProviderTerms,
        highlighted: bool,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let key = item.key();
        let id = format!("source-board-row:{key}");
        let look = state_look(item, self.status_of(item));
        let number = terms.number_label(item);
        let hover = palette.hover.opacity(0.6);
        let mut meta: Vec<AnyElement> = vec![
            div()
                .font_family("monospace")
                .child(SharedString::from(number.clone()))
                .into_any_element(),
        ];
        if let Some(author) = &item.author {
            meta.push(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(author.clone()))
                    .into_any_element(),
            );
        }
        if let Some(age) = age_label(&strings, item.updated_at.as_deref()) {
            meta.push(div().child(age).into_any_element());
        }
        if item.comment_count > 0 {
            meta.push(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(2.0))
                    .child(glyph("chat-1", 14.0, palette.muted_foreground))
                    .child(SharedString::from(item.comment_count.to_string()))
                    .into_any_element(),
            );
        }
        if item.upstream {
            meta.push(
                div()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .bg(palette.status_info.opacity(0.14))
                    .text_color(palette.status_info)
                    .child(SharedString::from(item.project_path()))
                    .into_any_element(),
            );
        }
        for label in item.labels.iter().take(ROW_LABELS) {
            meta.push(self.label_chip(&label.name, label.color.as_deref(), palette, direction));
        }
        if item.labels.len() > ROW_LABELS {
            meta.push(
                div()
                    .child(SharedString::from(format!(
                        "+{}",
                        item.labels.len() - ROW_LABELS
                    )))
                    .into_any_element(),
            );
        }
        on_activate(
            div()
                .id(SharedString::from(id.clone()))
                .role(Role::ListBoxOption)
                .accessibility_id(id)
                .aria_label(SharedString::from(format!("{number} {}", item.title)))
                .aria_selected(highlighted)
                .px(px(10.0))
                .py(px(10.0))
                .rounded(px(8.0))
                .flex_row_from(direction)
                .items_start()
                .gap(px(10.0))
                .cursor_pointer()
                .when(highlighted, |this| this.bg(palette.selection.opacity(0.7)))
                .when(!highlighted, |this| {
                    this.hover(move |style| style.bg(hover))
                })
                .child(div().pt(px(2.0)).child(glyph(
                    look.icon,
                    16.0,
                    tone_color(look.tone, palette),
                )))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .css_text(HEADER_SIZE, HEADER_LINE, cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if highlighted {
                                    palette.selection_foreground
                                } else {
                                    palette.foreground
                                })
                                .line_clamp(2)
                                .child(SharedString::from(item.title.clone())),
                        )
                        .child(
                            div()
                                .flex_row_from(direction)
                                .flex_wrap()
                                .items_center()
                                .gap_x(px(12.0))
                                .gap_y(px(2.0))
                                .css_text(MICRO_SIZE, MICRO_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .children(meta),
                        ),
                ),
            cx,
            move |this: &mut Self, _, cx| this.highlight(key.clone(), cx),
        )
        .test_support()
        .into_any_element()
    }

    fn label_chip(
        &self,
        name: &str,
        color: Option<&str>,
        palette: &Palette,
        direction: Direction,
    ) -> AnyElement {
        div()
            .max_w(px(144.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .size(px(6.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(label_color(color, palette)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(name.to_owned())),
            )
            .into_any_element()
    }

    fn meta_row(
        &self,
        label: SharedString,
        value: AnyElement,
        direction: Direction,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(16.0))
            .css_text(META_SIZE, META_LINE, cx)
            .child(
                div()
                    .w(px(96.0))
                    .flex_shrink_0()
                    .text_color(palette.muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(palette.foreground)
                    .child(value),
            )
            .into_any_element()
    }

    fn checks_line(&self, checks: &ChecksSummary, strings: &Strings) -> Option<SharedString> {
        if checks.total == 0 {
            return None;
        }
        let total = checks.total.to_string();
        Some(match checks.state {
            ChecksState::Failure => strings.format(
                "references.picker.preview.checksFailed",
                &[("failed", &checks.failure.to_string()), ("total", &total)],
            ),
            ChecksState::Pending => strings.format(
                "references.picker.preview.checksPending",
                &[("pending", &checks.pending.to_string()), ("total", &total)],
            ),
            ChecksState::Success => strings.get("references.picker.preview.checksPassed"),
            ChecksState::Unknown => return None,
        })
    }

    fn render_preview(
        &self,
        terms: &ProviderTerms,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let Some(item) = self.previewed().cloned() else {
            return self.centered(
                "source-board-preview-empty",
                strings.get("references.picker.preview.empty"),
                false,
                None,
                palette,
                cx,
            );
        };
        let status = self.status_of(&item).cloned();
        let look = state_look(&item, status.as_ref());
        let state_label = strings.get(look.label_key);
        let color = tone_color(look.tone, palette);
        let can_change = item.next_state().is_some() && self.busy.is_none();
        let pill = div()
            .h(px(24.0))
            .px(px(8.0))
            .rounded_full()
            .bg(color.opacity(0.14))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .child(glyph(look.icon, 14.0, color))
            .child(
                div()
                    .css_text(MICRO_SIZE, MICRO_LINE, cx)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color)
                    .child(state_label.clone()),
            )
            .when(can_change, |this| {
                this.child(glyph("arrow-down-s", 14.0, color))
            });
        let pill = if can_change {
            on_activate(
                div()
                    .id("source-board-state")
                    .role(Role::Button)
                    .accessibility_id("source-board-state")
                    .aria_label(strings.format(
                        "references.picker.preview.stateMenuAria",
                        &[("state", &state_label)],
                    ))
                    .aria_expanded(self.menu == Some(Menu::State))
                    .cursor_pointer()
                    .child(pill),
                cx,
                |this: &mut Self, _, cx| this.toggle_menu(Menu::State, cx),
            )
            .test_support()
            .into_any_element()
        } else {
            div()
                .id("source-board-state")
                .role(Role::Status)
                .accessibility_id("source-board-state")
                .aria_label(state_label.clone())
                .child(pill)
                .into_any_element()
        };
        let state_menu = (self.menu == Some(Menu::State)).then(|| {
            let next = item.next_state();
            let label = match (item.kind, next) {
                (
                    RepositoryKind::Issue,
                    Some(openchamber_native_core::source_board::TargetState::Closed),
                ) => strings.get("sourceBoard.actions.closeIssue"),
                (RepositoryKind::Issue, _) => strings.get("sourceBoard.actions.reopenIssue"),
                (
                    RepositoryKind::Pull,
                    Some(openchamber_native_core::source_board::TargetState::Closed),
                ) => strings.get(term_keys(&terms.change_request).close),
                (RepositoryKind::Pull, _) => strings.get(term_keys(&terms.change_request).reopen),
            };
            div()
                .id("source-board-state-menu")
                .role(Role::Menu)
                .accessibility_id("source-board-state-menu")
                .absolute()
                .top(px(32.0))
                .inset_start(direction, px(0.0))
                .w(px(224.0))
                .p(px(4.0))
                .rounded(px(10.0))
                .bg(palette.elevated)
                .shadow(elevation::floating(palette))
                .occlude()
                .child(self.menu_option(
                    "source-board-state-change".to_owned(),
                    label,
                    false,
                    palette,
                    direction,
                    cx,
                    |this: &mut Self, window, cx| this.change_state(window, cx),
                ))
        });
        let url = item.url.clone();
        let link = item.url.clone();
        let host_hover = palette.hover;
        let header = div()
            .relative()
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .child(pill)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .css_text(META_SIZE, META_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(SharedString::from(format!(
                        "{} {}",
                        item.project_path(),
                        terms.number_label(&item)
                    ))),
            )
            .child(div().flex_1())
            .child(
                IconButton::new(
                    "source-board-item-refresh",
                    if self.refreshing_detail {
                        "loader-4"
                    } else {
                        "refresh"
                    },
                    strings.get("references.picker.preview.refresh"),
                )
                .size(24.0, 14.0)
                .render(palette, cx, |this: &mut Self, _, cx| this.refresh_item(cx)),
            )
            .child(
                IconButton::new(
                    "source-board-copy-link",
                    "link",
                    strings.get("references.link.copy"),
                )
                .size(24.0, 14.0)
                .render(palette, cx, move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                }),
            )
            .child(
                on_activate(
                    div()
                        .id("source-board-open-host")
                        .role(Role::Link)
                        .accessibility_id("source-board-open-host")
                        .aria_label(SharedString::from(terms.label.clone()))
                        .h(px(24.0))
                        .px(px(6.0))
                        .rounded(px(6.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(4.0))
                        .cursor_pointer()
                        .hover(move |style| style.bg(host_hover))
                        .css_text(META_SIZE, META_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(SharedString::from(terms.label.clone()))
                        .child(glyph("external-link", 14.0, palette.muted_foreground)),
                    cx,
                    move |_, window, cx| crate::markdown_links::open_user_link(&url, window, cx),
                )
                .test_support(),
            )
            .children(state_menu);

        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(author) = &item.author {
            meta.push(
                self.meta_row(
                    strings.get("references.picker.preview.author"),
                    div()
                        .child(SharedString::from(author.clone()))
                        .into_any_element(),
                    direction,
                    palette,
                    cx,
                ),
            );
        }
        if let Some(age) = age_label(&strings, item.updated_at.as_deref()) {
            meta.push(self.meta_row(
                strings.get("references.picker.preview.updated"),
                div().child(age).into_any_element(),
                direction,
                palette,
                cx,
            ));
        }
        let detail_pull = match &self.detail {
            DetailLoad::Ready(detail) => detail.pull.clone(),
            _ => None,
        };
        if let Some(pull) = &item.pull {
            meta.push(
                self.meta_row(
                    strings.get("references.picker.preview.branch"),
                    div()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(4.0))
                        .font_family("monospace")
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(pull.head.clone())),
                        )
                        .child(glyph("arrow-right", 12.0, palette.muted_foreground))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(pull.base.clone())),
                        )
                        .into_any_element(),
                    direction,
                    palette,
                    cx,
                ),
            );
            let changes: AnyElement = match (&self.detail, &detail_pull) {
                (_, Some(size)) => div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_color(palette.status_success)
                            .child(SharedString::from(format!("+{}", size.additions))),
                    )
                    .child(
                        div()
                            .text_color(palette.status_error)
                            .child(SharedString::from(format!("−{}", size.deletions))),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(2.0))
                            .text_color(palette.muted_foreground)
                            .child(glyph("file-list-2", 14.0, palette.muted_foreground))
                            .child(SharedString::from(size.changed_files.to_string())),
                    )
                    .into_any_element(),
                (DetailLoad::Failed(error), None) => div()
                    .text_color(palette.status_error)
                    .child(strings.format("references.picker.error.load", &[("error", error)]))
                    .into_any_element(),
                _ => div()
                    .text_color(palette.muted_foreground)
                    .child("…")
                    .into_any_element(),
            };
            meta.push(self.meta_row(
                strings.get("references.picker.preview.changes"),
                changes,
                direction,
                palette,
                cx,
            ));
            let checks = status
                .as_ref()
                .and_then(|status| status.checks)
                .or_else(|| detail_pull.as_ref().and_then(|pull| pull.checks));
            if let Some(line) = checks
                .as_ref()
                .and_then(|checks| self.checks_line(checks, &strings))
            {
                meta.push(self.meta_row(
                    strings.get("references.picker.preview.checks"),
                    div().child(line).into_any_element(),
                    direction,
                    palette,
                    cx,
                ));
            }
            if let Some(decision) = detail_pull.as_ref().and_then(|pull| pull.review_decision) {
                meta.push(
                    self.meta_row(
                        strings.get("references.picker.preview.review"),
                        div()
                            .child(strings.get(decision.label_key()))
                            .into_any_element(),
                        direction,
                        palette,
                        cx,
                    ),
                );
            }
        }
        if !item.labels.is_empty() {
            meta.push(
                self.meta_row(
                    strings.get("references.picker.preview.labels"),
                    div()
                        .flex_row_from(direction)
                        .flex_wrap()
                        .gap(px(8.0))
                        .children(item.labels.iter().map(|label| {
                            self.label_chip(&label.name, label.color.as_deref(), palette, direction)
                        }))
                        .into_any_element(),
                    direction,
                    palette,
                    cx,
                ),
            );
        }
        let body: AnyElement = if item.body.trim().is_empty() {
            div()
                .css_text(META_SIZE, META_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(strings.get("references.picker.preview.noDescription"))
                .into_any_element()
        } else {
            markdown_view::markdown(
                &format!("source-board-body:{}", item.key()),
                &item.body,
                palette,
                Author::User,
            )
            .into_any_element()
        };
        div()
            .id("source-board-preview")
            .role(Role::Region)
            .accessibility_id("source-board-preview")
            .test_support()
            .aria_label(SharedString::from(item.title.clone()))
            .size_full()
            .overflow_y_scroll()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div().flex().flex_col().gap(px(8.0)).child(header).child(
                    div()
                        .id("source-board-title")
                        .role(Role::Heading)
                        .accessibility_id("source-board-title")
                        .aria_label(SharedString::from(item.title.clone()))
                        .css_text(HEADER_SIZE, HEADER_LINE, cx)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.foreground)
                        .child(SharedString::from(item.title.clone())),
                ),
            )
            .child(div().flex().flex_col().gap(px(6.0)).children(meta))
            .child(div().flex().flex_col().gap(px(12.0)).child(body).when(
                item.body_truncated,
                |this| {
                    this.child(
                        div()
                            .css_text(META_SIZE, META_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.get("references.picker.preview.truncated")),
                    )
                },
            ))
            .child(self.render_comments(&item, palette, direction, cx))
            .child(self.render_reply(&item, palette, direction, cx))
            .into_any_element()
    }

    fn render_comments(
        &self,
        item: &Reference,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let title = strings.get(if item.kind == RepositoryKind::Pull {
            "references.picker.preview.activity"
        } else {
            "references.picker.preview.comments"
        });
        let mut heading = div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .css_text(LABEL_SIZE, LABEL_LINE, cx)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.foreground)
                    .child(title.clone()),
            );
        let content: AnyElement = match &self.detail {
            DetailLoad::Ready(detail) => {
                let shown = detail.comments.len() as u64;
                if detail.comment_total > shown {
                    heading = heading.child(
                        div()
                            .css_text(META_SIZE, META_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.format(
                                "references.picker.preview.commentsLatest",
                                &[
                                    ("shown", &shown.to_string()),
                                    ("total", &detail.comment_total.to_string()),
                                ],
                            )),
                    );
                }
                if detail.comments.is_empty() {
                    div()
                        .css_text(META_SIZE, META_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(strings.get("references.picker.preview.commentsEmpty"))
                        .into_any_element()
                } else {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(16.0))
                        .children(detail.comments.iter().enumerate().map(|(index, comment)| {
                            self.render_comment(index, comment, &strings, palette, direction, cx)
                        }))
                        .into_any_element()
                }
            }
            DetailLoad::Failed(error) => div()
                .id("source-board-detail-error")
                .role(Role::Status)
                .accessibility_id("source-board-detail-error")
                .css_text(META_SIZE, META_LINE, cx)
                .text_color(palette.status_error)
                .child(strings.format("references.picker.error.load", &[("error", error)]))
                .into_any_element(),
            DetailLoad::Loading | DetailLoad::Idle => div()
                .css_text(META_SIZE, META_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(strings.get("references.picker.loading"))
                .into_any_element(),
        };
        div()
            .id("source-board-comments")
            .role(Role::Group)
            .accessibility_id("source-board-comments")
            .aria_label(title)
            .pt(px(16.0))
            .border_t_1()
            .border_color(palette.divider)
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(heading)
            .child(content)
            .into_any_element()
    }

    fn render_comment(
        &self,
        index: usize,
        comment: &Comment,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let author: SharedString = comment
            .author
            .clone()
            .unwrap_or_else(|| "—".to_owned())
            .into();
        // A review's verdict or the file a review comment is on.
        let context = comment
            .path
            .clone()
            .map(SharedString::from)
            .or(match comment.review {
                Some(CommentReview::Approved) => {
                    Some(strings.get("references.picker.preview.review.approved"))
                }
                Some(CommentReview::ChangesRequested) => {
                    Some(strings.get("references.picker.preview.review.changesRequested"))
                }
                Some(CommentReview::Dismissed) => {
                    Some(strings.get("references.picker.comment.dismissed"))
                }
                Some(CommentReview::Commented) | None => None,
            });
        let id = format!("source-board-comment:{index}");
        div()
            .id(SharedString::from(id.clone()))
            .role(Role::Article)
            .accessibility_id(id)
            .aria_label(SharedString::from(format!("{author}: {}", comment.body)))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .css_text(META_SIZE, META_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(div().font_weight(FontWeight::MEDIUM).child(author))
                    .children(age_label(strings, comment.created_at.as_deref()))
                    .children(context.map(|context| {
                        div()
                            .min_w_0()
                            .truncate()
                            .css_text(MICRO_SIZE, MICRO_LINE, cx)
                            .child(context)
                    })),
            )
            .child(markdown_view::markdown(
                &format!("source-board-comment-body:{index}:{}", comment.url),
                &comment.body,
                palette,
                Author::User,
            ))
            .into_any_element()
    }

    fn render_reply(
        &self,
        item: &Reference,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let has_text = !self.reply.read(cx).value().trim().is_empty();
        let idle = self.busy.is_none();
        let button = |id: &'static str,
                      icon: &'static str,
                      label: SharedString,
                      enabled: bool,
                      primary: bool,
                      busy: bool,
                      cx: &mut Context<Self>,
                      action: Action| {
            let (fill, text) = if primary {
                (palette.primary, palette.primary_foreground)
            } else {
                (palette.background, palette.foreground)
            };
            let hover = if primary {
                palette.primary_hover
            } else {
                palette.hover
            };
            on_activate(
                div()
                    .id(id)
                    .role(Role::Button)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .h(px(28.0))
                    .px(px(10.0))
                    .rounded(px(CONTROL_RADIUS))
                    .bg(fill)
                    .when(!primary, |this| this.shadow(elevation::control(palette)))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .when(enabled, |this| {
                        this.cursor_pointer().hover(move |style| style.bg(hover))
                    })
                    .when(!enabled, |this| this.opacity(0.5))
                    .child(glyph(if busy { "loader-4" } else { icon }, 14.0, text))
                    .child(
                        div()
                            .css_text(META_SIZE, META_LINE, cx)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(text)
                            .child(label),
                    ),
                cx,
                move |this: &mut Self, window, cx| {
                    if enabled {
                        action(this, window, cx);
                    }
                },
            )
            .test_support()
            .into_any_element()
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        if item.open_pull().is_some() {
            actions.push(button(
                "source-board-request-changes",
                "file-edit",
                strings.get("sourceBoard.reply.requestChanges"),
                idle && has_text,
                false,
                self.busy == Some(Busy::Review(ReviewVerdict::RequestChanges)),
                cx,
                Box::new(|this, window, cx| this.review(ReviewVerdict::RequestChanges, window, cx)),
            ));
            actions.push(button(
                "source-board-approve",
                "check",
                strings.get("sourceBoard.reply.approve"),
                idle,
                false,
                self.busy == Some(Busy::Review(ReviewVerdict::Approve)),
                cx,
                Box::new(|this, window, cx| this.review(ReviewVerdict::Approve, window, cx)),
            ));
        }
        actions.push(button(
            "source-board-comment",
            "chat-1",
            strings.get("sourceBoard.reply.comment"),
            idle && has_text,
            true,
            self.busy == Some(Busy::Comment),
            cx,
            Box::new(|this, window, cx| this.comment(window, cx)),
        ));
        div()
            .id("source-board-reply-section")
            .role(Role::Group)
            .accessibility_id("source-board-reply-section")
            .aria_label(strings.get("sourceBoard.reply.label"))
            .pt(px(16.0))
            .border_t_1()
            .border_color(palette.divider)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .rounded(px(CONTROL_RADIUS))
                    .bg(palette.background)
                    .shadow(elevation::control(palette))
                    .child(
                        Textarea::new(&self.reply)
                            .disabled(!idle)
                            .px(px(8.0))
                            .py(px(6.0))
                            .text_size(rems(LABEL_SIZE))
                            .accessibility_id("source-board-reply")
                            .aria_label(strings.get("sourceBoard.reply.label")),
                    ),
            )
            .child(
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .justify_end()
                    .gap(px(8.0))
                    .children(actions),
            )
            .into_any_element()
    }
}

impl Render for SourceBoard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let palette = cx.global::<Palette>().clone();
        let term = self
            .context()
            .map(|(_, terms)| terms.change_request.clone())
            .unwrap_or_else(|| "pullRequest".to_owned());
        let placeholder = strings.get(term_keys(&term).search);
        if self.search.read(cx).presentation().placeholder() != &placeholder {
            self.search.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        let reply_placeholder = strings.get("sourceBoard.reply.placeholder");
        if self.reply.read(cx).presentation().placeholder() != &reply_placeholder {
            self.reply.update(cx, |input, cx| {
                input.set_placeholder(reply_placeholder, window, cx)
            });
        }
        // A draft never carries over to another item.
        if self.take_reply_reset() {
            self.reply
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        let body = match self.render_source_state(&palette, cx) {
            Some(state) => state,
            None => {
                let terms = self
                    .context()
                    .map(|(_, terms)| terms.clone())
                    .expect("a ready source has terms");
                div()
                    .size_full()
                    .flex_row_from(direction)
                    .child(
                        div()
                            .h_full()
                            .w(gpui_kit::relative(0.416_66))
                            .min_w(px(LIST_MIN_WIDTH))
                            .max_w(gpui_kit::relative(0.6))
                            .flex_shrink_0()
                            .child(self.render_list(&terms, &palette, direction, cx)),
                    )
                    .child(
                        div()
                            .w(px(1.0))
                            .h_full()
                            .flex_shrink_0()
                            .bg(palette.divider),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_preview(&terms, &palette, direction, cx)),
                    )
                    .into_any_element()
            }
        };
        if self.list.has_more() && !self.list.is_loading_more() {
            let entity = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = entity.update(cx, |this, cx| this.maybe_load_more(cx));
            });
        }
        div()
            .id("source-board-page")
            .role(Role::Region)
            .accessibility_id("source-board-page")
            .test_support()
            .aria_label(strings.get("sourceBoard.title"))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.foreground)
            .child(self.render_toolbar(&palette, direction, cx))
            .child(status_line(
                "source-board-notice",
                self.notice.as_ref(),
                &palette,
                direction,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
            .children(self.render_menu(&palette, direction, cx))
    }
}
