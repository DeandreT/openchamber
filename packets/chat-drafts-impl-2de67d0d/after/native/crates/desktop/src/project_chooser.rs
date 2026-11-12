//! The target of a client-owned new-session draft: Chat, or a registered
//! project (`DraftTargetSelectors.tsx`, whose list starts with Chat).

use gpui_kit::component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::{
    AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, StyleRefinement, Styled, Subscription,
    TestSupportExt, Window, div, px,
};
use openchamber_native_core::chat_draft::CHAT_PROJECT_ID;

use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::connection::ConnectionState;
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::send_catalog::Load;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The trigger's project mark. `ProjectLabel` asks for 14 px, but the
/// `Button` it sits in sizes every icon without a `size-*` class to 16 px.
pub(crate) const ICON_ID: &str = "choose-project-icon";
const ICON_SIZE: f32 = 16.0;

/// The Chat row's label. The reference writes `layout.mainTab.chat` ("Chat"),
/// which the native key list does not carry yet.
const CHAT_LABEL: &str = "sessions.sidebar.activity.chatsTitle";

/// Selects a registered project, or Chat by `CHAT_PROJECT_ID`, through the
/// popup's accessibility action.
#[derive(Clone, PartialEq, serde::Deserialize, gpui_kit::Action)]
#[action(namespace = openchamber, no_json)]
pub(crate) struct ChooseDraftProject {
    pub(crate) directory: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProjectOption {
    directory: String,
    label: SharedString,
}

impl SearchableListItem for ProjectOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.directory
    }
    fn render(&self, _: &mut Window, cx: &mut gpui_kit::App) -> impl IntoElement {
        let palette = cx.global::<Palette>();
        let directory = self.directory.clone();
        let icon = if self.directory == CHAT_PROJECT_ID {
            "chat-new"
        } else {
            "folder"
        };
        div()
            .id(SharedString::from(format!(
                "project-option:{}",
                self.directory
            )))
            .role(Role::ListBoxOption)
            .accessibility_id(format!("project-option:{}", self.directory))
            .aria_label(self.label.clone())
            .on_a11y_action(gpui_kit::AccessibleAction::Click, move |_, window, cx| {
                window.dispatch_action(
                    Box::new(ChooseDraftProject {
                        directory: directory.clone(),
                    }),
                    cx,
                );
            })
            .flex_row_from(locale::strings(cx).direction())
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .child(glyph(icon, 14.0, palette.muted_foreground))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .css_text(0.84375, 1.25, cx)
                    .child(self.label.clone()),
            )
            .test_support()
    }
    fn matches(&self, query: &str) -> bool {
        let label = self.label.to_lowercase();
        let directory = self.directory.to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|token| label.contains(token) || directory.contains(token))
    }
}

pub(crate) struct ProjectChooser {
    items: Vec<ProjectOption>,
    pub(crate) state: Entity<ComboboxState<SearchableVec<ProjectOption>>>,
    _subscription: Subscription,
}

impl Workspace {
    /// A registered project's row: its saved label, else its folder name.
    fn project_option(
        &self,
        project: &openchamber_native_core::ownership::ConfiguredProject,
    ) -> ProjectOption {
        let label = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| {
                settings
                    .project_labels
                    .iter()
                    .find(|entry| entry.path == project.path)
            })
            .map(|entry| entry.label.clone())
            .unwrap_or_else(|| {
                project
                    .path
                    .rsplit('/')
                    .find(|segment| !segment.is_empty())
                    .unwrap_or(&project.path)
                    .to_owned()
            });
        ProjectOption {
            directory: project.path.clone(),
            label: label.into(),
        }
    }

    pub(crate) fn render_project_chooser(
        &mut self,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let strings = locale::strings(cx).clone();
        // Chat leads the list wherever a server can create one.
        let chat = (!self.is_synthetic()).then(|| ProjectOption {
            directory: CHAT_PROJECT_ID.to_owned(),
            label: strings.get(CHAT_LABEL),
        });
        let items: Vec<ProjectOption> = chat
            .into_iter()
            .chain(
                self.topology
                    .projects
                    .iter()
                    .filter(|project| {
                        openchamber_native_core::path::normalize(&project.path).is_some()
                    })
                    .map(|project| self.project_option(project)),
            )
            .collect();
        if self.project_chooser.is_none() {
            let state = cx.new(|cx| {
                ComboboxState::new(SearchableVec::new(items.clone()), vec![], window, cx)
                    .searchable(true)
                    .confirm_on_reselect(true)
            });
            let subscription = cx.subscribe_in(&state, window, |this, _, event, window, cx| {
                if let ComboboxEvent::Confirm(values) = event
                    && let Some(directory) = values.first()
                {
                    // The open draft changes where it runs and keeps its text.
                    this.move_draft_to(directory, window, cx);
                }
            });
            self.project_chooser = Some(ProjectChooser {
                items: items.clone(),
                state,
                _subscription: subscription,
            });
        }
        let chat_choice = self.chat_draft_open().then(|| CHAT_PROJECT_ID.to_owned());
        let chooser = self
            .project_chooser
            .as_mut()
            .expect("the chooser was initialized");
        if chooser.items != items {
            chooser.state.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(items.clone()), window, cx)
            });
            chooser.items = items;
        }
        let directory = self
            .draft_projects
            .get(&self.runtime)
            .or(chat_choice.as_ref())
            .filter(|directory| {
                chooser
                    .items
                    .iter()
                    .any(|item| &item.directory == *directory)
            });
        if chooser.state.read(cx).selected_value().as_ref() != directory {
            chooser.state.update(cx, |state, cx| {
                state.set_selected_values(
                    &directory.cloned().into_iter().collect::<Vec<_>>(),
                    window,
                    cx,
                )
            });
        }
        let label = directory
            .and_then(|directory| {
                chooser
                    .items
                    .iter()
                    .find(|item| &item.directory == directory)
            })
            .map(|item| item.label.clone())
            .unwrap_or_else(|| strings.get("chat.chatInput.chooseProject"));
        // A project shows its folder mark; Chat is a bare word.
        let has_project = directory.is_some_and(|directory| directory != CHAT_PROJECT_ID);
        let connection_failed = self
            .connection
            .as_ref()
            .is_some_and(|connection| matches!(connection.state, ConnectionState::Failed(_)));
        let status = project_list_status(
            self.is_synthetic(),
            &self.send_catalog.settings_load,
            connection_failed,
        );
        let state = self
            .project_chooser
            .as_ref()
            .expect("the chooser was initialized")
            .state
            .clone();
        let empty = strings.get(status);
        let foreground = palette.foreground;
        let muted = palette.muted_foreground;
        let accessible_state = state.clone();
        div().key_context("ProjectChooser").h(px(28.0)).child(
            Combobox::new(&state)
                .placeholder(strings.get("chat.chatInput.chooseProject"))
                .appearance(false)
                .border_0()
                .p_0()
                .w_auto()
                .h(px(28.0))
                .menu_width(px(288.0))
                .menu_offset(px(4.0))
                .menu_align(if direction == Direction::RightToLeft {
                    gpui_kit::base::Align::End
                } else {
                    gpui_kit::base::Align::Start
                })
                .menu_style(
                    StyleRefinement::default()
                        .border_0()
                        .rounded(px(12.0))
                        .bg(palette.elevated)
                        .shadow(elevation::floating(palette)),
                )
                .search_clearable(false)
                .check_icon(
                    gpui_kit::component::Icon::default().path(crate::assets::icon_path("check")),
                )
                .search_placeholder(strings.get("chat.chatInput.draftPicker.searchProjects"))
                .empty(move |_, cx| {
                    div()
                        .px(px(12.0))
                        .py(px(24.0))
                        .text_center()
                        .text_color(muted)
                        .css_text(0.84375, 1.25, cx)
                        .child(empty.clone())
                })
                .render_trigger(move |trigger, _, cx| {
                    let accessible_state = accessible_state.clone();
                    let open = trigger.is_open();
                    div()
                        .id("choose-project")
                        .role(Role::Button)
                        .accessibility_id("choose-project")
                        .aria_label(label.clone())
                        .aria_expanded(open)
                        .on_a11y_action(gpui_kit::AccessibleAction::Click, move |_, window, cx| {
                            accessible_state.update(cx, |state, cx| state.focus(window, cx));
                            if open {
                                window
                                    .dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
                            } else {
                                window.dispatch_action(
                                    Box::new(gpui_kit::base::actions::Confirm { secondary: false }),
                                    cx,
                                );
                            }
                        })
                        .h(px(28.0))
                        .px(px(6.0))
                        .max_w(px(288.0))
                        .rounded(px(CONTROL_RADIUS))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(foreground)
                        .children(has_project.then(|| {
                            div()
                                .id(ICON_ID)
                                .size(px(ICON_SIZE))
                                .flex_shrink_0()
                                .child(glyph("folder", ICON_SIZE, muted))
                                .test_support()
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .css_text(0.84375, 1.25, cx)
                                .child(label.clone()),
                        )
                        .child(glyph("arrow-down-s", 16.0, foreground).opacity(0.5))
                        .test_support()
                }),
        )
    }
}

fn project_list_status(synthetic: bool, load: &Load<()>, connection_failed: bool) -> &'static str {
    match (synthetic, load) {
        (true, _) | (_, Load::Ready(())) => "chat.chatInput.draftPicker.noProjectsFound",
        (_, Load::Failed) => "common.unavailable",
        (_, Load::Loading) if connection_failed => "common.unavailable",
        (_, Load::Loading) => "common.loading",
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt;

    use super::*;

    #[gpui_kit::test]
    fn the_trigger_draws_the_project_mark_at_the_button_icon_size(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let directory = workspace.topology.projects[0].path.clone();
                workspace.choose_draft_project(&directory, window, cx);
            });
            window.render_frame(cx);
            let trigger = window.find("choose-project").bounds();
            let icon = window.find(ICON_ID).bounds();
            assert_eq!(trigger.size.height, px(28.0), "the trigger is h-7");
            assert_eq!(
                icon.size,
                gpui_kit::size(px(16.0), px(16.0)),
                "a Button sizes its icons to 16px"
            );
            assert_eq!(
                icon.left() - trigger.left(),
                px(6.0),
                "px-1.5 before the mark"
            );
            assert_eq!(
                icon.top() - trigger.top(),
                px(6.0),
                "the mark is centered in the 28px trigger"
            );
        })
        .expect("window is open");
    }

    #[test]
    fn failed_project_discovery_is_not_an_empty_list() {
        assert_eq!(
            project_list_status(false, &Load::Loading, false),
            "common.loading"
        );
        assert_eq!(
            project_list_status(false, &Load::Loading, true),
            "common.unavailable"
        );
        assert_eq!(
            project_list_status(false, &Load::Failed, false),
            "common.unavailable"
        );
        assert_eq!(
            project_list_status(false, &Load::Ready(()), false),
            "chat.chatInput.draftPicker.noProjectsFound"
        );
        assert_eq!(
            project_list_status(true, &Load::Loading, false),
            "chat.chatInput.draftPicker.noProjectsFound"
        );
    }
}
