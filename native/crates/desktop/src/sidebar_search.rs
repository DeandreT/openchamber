//! The sidebar's inline session search: its quiet toolbar field narrows
//! every section, Chats and the
//! zones to the matching sessions with their ancestors, and opens every
//! container while the query lasts. Escape first clears a filled field;
//! Escape on an empty field returns focus to the composer. Drafts are left
//! out of a search, which matches sessions.

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, EntityInputHandler, Focusable, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Role, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::grouping::{ProjectSection, SessionGroup};
use openchamber_native_core::session_search::{self, SearchQuery};

use crate::activation::on_activate;
use crate::chrome::glyph;
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The retained toolbar search field.
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
    /// Creates the retained toolbar input without taking keyboard focus.
    pub(crate) fn ensure_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = locale::strings(cx).get("settings.common.sidebar.searchPlaceholder");
        if let Some(search) = &self.session_management.search {
            if search.input.read(cx).presentation().placeholder() != &placeholder {
                search.input.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx)
                });
            }
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                let value = input.read(cx).value().to_string();
                this.search_sessions(&value, cx);
            }
            if matches!(event, InputEvent::Focus | InputEvent::Blur) {
                cx.notify();
            }
        });
        self.session_management.search = Some(SidebarSearch {
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// The search shortcut focuses the same field the toolbar displays.
    pub(crate) fn toggle_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_sidebar_search(window, cx);
        if let Some(search) = &self.session_management.search {
            search.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    fn clear_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.session_management.search {
            search.input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
        }
        self.search_sessions("", cx);
        cx.notify();
    }

    /// Leaves the empty field visible at rest, without retaining a hidden query.
    pub(crate) fn close_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_sidebar_search(window, cx);
        self.focus_composer(window, cx);
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

    /// The desktop field shares the toolbar row and keeps its size at rest.
    pub(crate) fn render_sidebar_search(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let search = self.session_management.search.as_ref()?;
        let strings = locale::strings(cx).clone();
        let input = search.input.clone();
        let filled = !input.read(cx).value().is_empty();
        let focused = input.read(cx).focus_handle(cx).is_focused(window);
        let clear_label = strings.get("sessions.sidebar.header.search.clear");
        let clear = on_activate(
            div()
                .id("sidebar-search-clear")
                .role(Role::Button)
                .accessibility_id("sidebar-search-clear")
                .aria_label(clear_label)
                .size(px(24.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(glyph("close", 14.0, palette.muted_foreground)),
            cx,
            |this, window, cx| this.clear_sidebar_search(window, cx),
        )
        .test_support();
        let escape_input = input.clone();
        Some(
            div()
                .id("sidebar-search")
                .role(Role::Search)
                .accessibility_id("sidebar-search")
                .flex_1()
                .min_w_0()
                .h(px(32.0))
                .padding_start(direction, px(6.0))
                .padding_end(direction, px(4.0))
                .rounded(px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .when(focused || filled, |field| {
                    field
                        .bg(palette.elevated)
                        .shadow(elevation::control(palette))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.toggle_sidebar_search(window, cx);
                    }),
                )
                // IME Escape belongs to the composition, never to search.
                .capture_action(cx.listener(move |this, _: &Escape, window, cx| {
                    let composing = escape_input.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    if !composing {
                        cx.stop_propagation();
                        if !escape_input.read(cx).value().is_empty() {
                            this.clear_sidebar_search(window, cx);
                        } else {
                            this.close_sidebar_search(window, cx);
                        }
                    }
                }))
                .child(glyph("search", 16.0, palette.muted_foreground))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&input)
                            .id("search-sessions")
                            .accessibility_id("search-sessions")
                            .aria_label(
                                strings.get("sessions.sidebar.header.actions.searchSessions"),
                            )
                            .appearance(false)
                            .bordered(false)
                            .text_size(gpui_kit::rems(0.875)),
                    ),
                )
                .when(filled, |field| field.child(clear))
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
