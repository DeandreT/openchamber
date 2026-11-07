//! The sidebar's session search: the toolbar's Search sessions button opens
//! a field under the toolbar; typing narrows every section, Chats and the
//! zones to the matching sessions with their ancestors, and opens every
//! container while the query lasts. Escape or Clear search empties the field
//! and closes it. Drafts are left out of a search, which matches sessions.

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, EntityInputHandler, InteractiveElement, IntoElement,
    ParentElement, Role, StatefulInteractiveElement, Styled, Subscription, TestSupportExt, Window,
    div, prelude::FluentBuilder, px,
};
use openchamber_native_core::grouping::{ProjectSection, SessionGroup};
use openchamber_native_core::session_search::{self, SearchQuery};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::sidebar_item::SidebarItem;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The open search field.
pub(crate) struct SidebarSearch {
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// Narrows the grouped sections and Chats to `query`'s matches; a project
/// with no match leaves the list.
pub(crate) fn filter_sections(
    sections: &mut Vec<ProjectSection<'_>>,
    chat_group: Option<&mut SessionGroup<'_>>,
    query: &SearchQuery,
) {
    for section in sections.iter_mut() {
        for group in &mut section.groups {
            group.sessions = session_search::filter_nodes(&group.sessions, query);
        }
    }
    sections.retain(|section| {
        section
            .groups
            .iter()
            .any(|group| !group.sessions.is_empty())
    });
    if let Some(chats) = chat_group {
        chats.sessions = session_search::filter_nodes(&chats.sessions, query);
    }
}

impl Workspace {
    /// The toolbar's Search sessions: opens the field, or closes and clears
    /// it.
    pub(crate) fn toggle_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.search.is_some() {
            self.close_sidebar_search(window, cx);
            return;
        }
        let placeholder = locale::strings(cx).get("sessions.sidebar.header.search.placeholder");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.update(cx, |input, cx| input.focus(window, cx));
        let subscription = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                let value = input.read(cx).value().to_string();
                this.search_sessions(&value, cx);
            }
        });
        self.session_management.search = Some(SidebarSearch {
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Empties and closes the field; the list shows everything again.
    pub(crate) fn close_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.search.take().is_some() {
            self.sidebar_state.search = None;
            self.rebuild_sidebar();
            self.focus_composer(window, cx);
            cx.notify();
        }
    }

    fn search_sessions(&mut self, value: &str, cx: &mut Context<Self>) {
        let untitled = locale::strings(cx).get("sessions.sidebar.session.untitled");
        let query = SearchQuery::parse(value, &untitled);
        if self.sidebar_state.search == query {
            return;
        }
        self.sidebar_state.search = query;
        self.rebuild_sidebar();
        cx.notify();
    }

    /// The search field under the toolbar, with the match count.
    pub(crate) fn render_sidebar_search(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let search = self.session_management.search.as_ref()?;
        let strings = locale::strings(cx).clone();
        let input = search.input.clone();
        let matches = self
            .sidebar_items
            .iter()
            .filter(|item| matches!(item, SidebarItem::Session { .. }))
            .count();
        let count = self.sidebar_state.search.as_ref().map(|_| {
            let key = if matches == 1 {
                "sessions.sidebar.header.search.matchCountSingle"
            } else {
                "sessions.sidebar.header.search.matchCountPlural"
            };
            strings.format(key, &[("count", &matches.to_string())])
        });
        let clear_label = strings.get("sessions.sidebar.header.search.clear");
        let clear = on_activate(
            div()
                .id("sidebar-search-clear")
                .role(Role::Button)
                .accessibility_id("sidebar-search-clear")
                .aria_label(clear_label)
                .cursor_pointer()
                .child(glyph("close", 14.0, palette.muted_foreground)),
            cx,
            |this, window, cx| this.close_sidebar_search(window, cx),
        )
        .test_support();
        let escape_input = input.clone();
        Some(
            div()
                .id("sidebar-search")
                .role(Role::Search)
                .accessibility_id("sidebar-search")
                .flex_shrink_0()
                .mx(px(10.0))
                .mb(px(6.0))
                .px(px(8.0))
                .rounded(px(8.0))
                .bg(palette.background)
                .border_1()
                .border_color(palette.border)
                .flex()
                .flex_col()
                // Escape clears the search unless it ends an IME composition.
                .capture_action(cx.listener(move |this, _: &Escape, window, cx| {
                    let composing = escape_input.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    if !composing {
                        cx.stop_propagation();
                        this.close_sidebar_search(window, cx);
                    }
                }))
                .child(
                    div()
                        .h(px(28.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .child(glyph("search", 14.0, palette.muted_foreground))
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&input)
                                    .appearance(false)
                                    .bordered(false)
                                    .text_size(gpui_kit::rems(0.843_75)),
                            ),
                        )
                        .child(clear),
                )
                .when_some(count, |this, count| {
                    this.child(
                        div()
                            .id("sidebar-search-count")
                            .role(Role::Status)
                            .accessibility_id("sidebar-search-count")
                            .aria_label(count.clone())
                            .pb(px(4.0))
                            .flex_row_from(direction)
                            .justify_between()
                            .css_text(0.75, 1.333_333, cx)
                            .text_color(palette.muted_foreground)
                            .child(count)
                            .child(strings.get("sessions.sidebar.header.search.escapeHint"))
                            .test_support(),
                    )
                })
                .test_support()
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::grouping::{DraftTarget, GroupLabel, SessionNode};
    use openchamber_native_core::ids::ProjectId;
    use openchamber_native_core::ownership::ConfiguredProject;
    use openchamber_native_core::session::SessionRecord;

    #[test]
    fn projects_without_a_match_leave_the_list() {
        let mut first = SessionRecord::new("a", "/a");
        first.title = "Relay limits".to_owned();
        let mut second = SessionRecord::new("b", "/b");
        second.title = "Docs".to_owned();
        let projects = [
            ConfiguredProject {
                id: ProjectId::from("a"),
                path: "/a".to_owned(),
            },
            ConfiguredProject {
                id: ProjectId::from("b"),
                path: "/b".to_owned(),
            },
        ];
        let group = |session| SessionGroup {
            id: "root".to_owned(),
            label: GroupLabel::ProjectRoot { branch: None },
            directory: None,
            is_main: true,
            is_archived_bucket: false,
            space: None,
            draft_target: DraftTarget::Project,
            sessions: vec![SessionNode {
                session,
                children: Vec::new(),
            }],
        };
        let mut sections = vec![
            ProjectSection {
                project: &projects[0],
                groups: vec![group(&first)],
            },
            ProjectSection {
                project: &projects[1],
                groups: vec![group(&second)],
            },
        ];
        let query = SearchQuery::parse("relay", "Untitled").expect("a query");
        filter_sections(&mut sections, None, &query);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].project.path, "/a");
    }
}
