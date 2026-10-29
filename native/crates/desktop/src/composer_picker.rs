//! Searchable native composer pickers using Kit's retained list and IME handling.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::IndexPath;
use gpui_kit::component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_kit::component::searchable_list::{
    SearchableListChange, SearchableListDelegate, SearchableListItem, SearchableVec,
};
use gpui_kit::{
    App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, Role, SharedString, StatefulInteractiveElement, StyleRefinement, Styled,
    Subscription, TestSupportExt, WeakEntity, Window, div, px,
};
use openchamber_native_core::composer_selection::{Effort, SelectionOwner};
use openchamber_native_core::send_config::{self, AgentMode, AutoRouting, ModelChoice};

use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickerKind {
    Model,
    Agent,
    Effort,
}

impl PickerKind {
    fn id(self) -> &'static str {
        match self {
            Self::Model => "composer-model",
            Self::Agent => "composer-agent",
            Self::Effort => "composer-variant",
        }
    }
    fn search(self) -> &'static str {
        match self {
            Self::Model => "chat.modelControls.searchModels",
            Self::Agent => "chat.modelControls.searchAgents",
            Self::Effort => "chat.modelControls.default",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pick {
    Model(ModelChoice),
    Agent(String),
    Effort(Effort),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PickValue {
    owner: SelectionOwner,
    pick: Pick,
}

#[derive(Clone)]
pub(crate) struct PickerOption {
    value: PickValue,
    id: SharedString,
    label: SharedString,
    search: String,
    current: bool,
    workspace: WeakEntity<Workspace>,
}

impl PartialEq for PickerOption {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
            && self.label == other.label
            && self.search == other.search
            && self.current == other.current
    }
}
impl Eq for PickerOption {}

impl SearchableListItem for PickerOption {
    type Value = PickValue;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &PickValue {
        &self.value
    }
    fn matches(&self, query: &str) -> bool {
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| self.search.contains(word))
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let value = self.value.clone();
        div()
            .id(self.id.clone())
            .accessibility_id(self.id.clone())
            .role(Role::RadioButton)
            .aria_label(self.label.clone())
            .aria_toggled(if self.current {
                gpui_kit::Toggled::True
            } else {
                gpui_kit::Toggled::False
            })
            .on_a11y_action(gpui_kit::AccessibleAction::Click, move |_, window, cx| {
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.apply_composer_pick(&value, cx)
                });
                window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
            })
            .min_w_0()
            .truncate()
            .css_text(0.84375, 1.25, cx)
            .child(self.label.clone())
            .test_support()
    }
}

pub(crate) struct Picker {
    state: Entity<ComboboxState<PickerItems>>,
    items: Rc<RefCell<Vec<PickerOption>>>,
    confirmed: Rc<RefCell<Option<PickValue>>>,
    _subscription: Subscription,
}

/// Kit emits Confirm for both selection and dismissal, and its Change signal
/// compares row indices. A filtered row can reuse the selected row's index.
/// This delegate records only an actual selection gesture, including reselect.
struct PickerItems {
    items: SearchableVec<PickerOption>,
    confirmed: Rc<RefCell<Option<PickValue>>>,
}

impl SearchableListDelegate for PickerItems {
    type Item = PickerOption;
    fn items_count(&self, section: usize) -> usize {
        self.items.items_count(section)
    }
    fn item(&self, index: IndexPath) -> Option<&PickerOption> {
        self.items.item(index)
    }
    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.items.position(value)
    }
    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui_kit::Task<()> {
        self.items.perform_search(query, window, cx)
    }
    fn on_will_change(
        &mut self,
        selection: &mut Vec<(IndexPath, PickerOption)>,
        changes: &[SearchableListChange],
    ) {
        self.items.on_will_change(selection, changes);
        *self.confirmed.borrow_mut() = selection.first().map(|(_, item)| item.value.clone());
    }
}

impl Picker {
    pub(crate) fn new(kind: PickerKind, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        if kind == PickerKind::Model {
            cx.bind_keys([
                gpui_kit::KeyBinding::new(
                    "ctrl-n",
                    gpui_kit::base::actions::SelectDown,
                    Some("ComposerPicker > List"),
                ),
                gpui_kit::KeyBinding::new(
                    "ctrl-p",
                    gpui_kit::base::actions::SelectUp,
                    Some("ComposerPicker > List"),
                ),
            ]);
        }
        let confirmed = Rc::new(RefCell::new(None));
        let state = cx.new(|cx| {
            ComboboxState::new(
                PickerItems {
                    items: SearchableVec::new(Vec::<PickerOption>::new()),
                    confirmed: confirmed.clone(),
                },
                vec![],
                window,
                cx,
            )
            .searchable(kind != PickerKind::Effort)
            .confirm_on_reselect(true)
        });
        let confirmation = confirmed.clone();
        let subscription =
            cx.subscribe_in(&state, window, move |workspace, _, event, window, cx| {
                if matches!(event, ComboboxEvent::Confirm(_)) {
                    if let Some(value) = confirmation.borrow_mut().take() {
                        workspace.apply_composer_pick(&value, cx);
                    }
                    cx.defer_in(window, |workspace, window, cx| {
                        workspace.focus_composer(window, cx)
                    });
                }
            });
        Self {
            state,
            items: Rc::default(),
            confirmed,
            _subscription: subscription,
        }
    }
}

impl Workspace {
    fn apply_composer_pick(&mut self, value: &PickValue, cx: &mut Context<Self>) {
        match &value.pick {
            Pick::Model(model) => self.choose_composer_model(&value.owner, model.clone(), cx),
            Pick::Agent(agent) => self.choose_composer_agent(&value.owner, agent, cx),
            Pick::Effort(effort) => self.choose_composer_effort(&value.owner, effort.clone(), cx),
        }
    }

    pub(crate) fn picker_control(
        &self,
        kind: PickerKind,
        appearance: PickerAppearance,
        show_label: bool,
        direction: Direction,
        cx: &Context<Self>,
    ) -> SelectionControl {
        let PickerAppearance {
            label,
            icon,
            color,
            provider,
        } = appearance;
        let picker = match kind {
            PickerKind::Model => &self.model_picker,
            PickerKind::Agent => &self.agent_picker,
            PickerKind::Effort => &self.effort_picker,
        };
        let mut items = Vec::new();
        if let (Some(owner), Some(inputs)) = (self.selection_owner(), self.choice_inputs()) {
            let selected = self
                .composer_selections
                .get(&owner)
                .map(|state| state.selection())
                .unwrap_or_default();
            let mut add =
                |pick: Pick, key: String, label: String, search: String, current: bool| {
                    items.push(PickerOption {
                        value: PickValue {
                            owner: owner.clone(),
                            pick,
                        },
                        id: format!("{}-option:{key}", kind.id()).into(),
                        label: label.into(),
                        search: search.to_lowercase(),
                        current,
                        workspace: cx.entity().downgrade(),
                    });
                };
            match kind {
                PickerKind::Model => {
                    if inputs.defaults.auto_routing == AutoRouting::Ready {
                        let label = locale::strings(cx)
                            .get("chat.modelControls.autoModel")
                            .to_string();
                        add(
                            Pick::Model(ModelChoice {
                                provider_id: "openchamber".into(),
                                model_id: "auto".into(),
                                variant: None,
                            }),
                            "openchamber/auto".into(),
                            label.clone(),
                            label,
                            selected.model.as_ref().is_some_and(|model| {
                                model.provider_id == "openchamber" && model.model_id == "auto"
                            }),
                        );
                    }
                    for provider in inputs.providers {
                        for model in &provider.models {
                            if inputs
                                .defaults
                                .hidden_models
                                .contains(&(provider.id.clone(), model.id.clone()))
                            {
                                continue;
                            }
                            let choice = ModelChoice {
                                provider_id: provider.id.clone(),
                                model_id: model.id.clone(),
                                variant: None,
                            };
                            let label = send_config::model_label(inputs.providers, &choice);
                            let key = format!("{}/{}", provider.id, model.id);
                            let search = format!(
                                "{label} {key} {}",
                                provider.name.as_deref().unwrap_or_default()
                            );
                            let current = selected.model.as_ref().is_some_and(|selected| {
                                selected.provider_id == choice.provider_id
                                    && selected.model_id == choice.model_id
                            });
                            add(Pick::Model(choice), key, label, search, current);
                        }
                    }
                }
                PickerKind::Agent => {
                    for agent in inputs.agents {
                        if agent.hidden || agent.mode == Some(AgentMode::Subagent) {
                            continue;
                        }
                        let label = send_config::agent_label(agent);
                        add(
                            Pick::Agent(agent.name.clone()),
                            agent.name.clone(),
                            label.clone(),
                            format!("{label} {}", agent.name),
                            selected.agent_name.as_ref() == Some(&agent.name),
                        );
                    }
                }
                PickerKind::Effort => {
                    if let Some(choice) = &selected.model
                        && let Some(model) = send_config::find_model(inputs.providers, choice)
                    {
                        let label = locale::strings(cx)
                            .get("chat.modelControls.default")
                            .to_string();
                        add(
                            Pick::Effort(Effort::Default),
                            "default".into(),
                            label.clone(),
                            label,
                            choice.variant.is_none(),
                        );
                        for variant in &model.variants {
                            add(
                                Pick::Effort(Effort::Named(variant.clone())),
                                format!("named:{variant}"),
                                variant.clone(),
                                variant.clone(),
                                choice.variant.as_ref() == Some(variant),
                            );
                        }
                    }
                }
            }
        }
        let directory = self.config_directory();
        let catalog = directory
            .as_deref()
            .and_then(|directory| self.send_catalog.directory(directory));
        let availability = match kind {
            PickerKind::Model => catalog.map(|entry| match entry.providers {
                crate::send_catalog::Load::Loading => "common.loading",
                crate::send_catalog::Load::Failed => "common.unavailable",
                crate::send_catalog::Load::Ready(_) => "chat.modelControls.noModelsFound",
            }),
            PickerKind::Agent => catalog.map(|entry| match entry.agents {
                crate::send_catalog::Load::Loading => "common.loading",
                crate::send_catalog::Load::Failed => "common.unavailable",
                crate::send_catalog::Load::Ready(_) => "commandPalette.empty.noResults",
            }),
            PickerKind::Effort => Some("commandPalette.empty.noResults"),
        };
        SelectionControl {
            kind,
            label,
            icon,
            color,
            provider,
            show_label,
            direction,
            state: picker.state.clone(),
            previous_items: picker.items.clone(),
            confirmed: picker.confirmed.clone(),
            items,
            workspace: cx.entity().downgrade(),
            disabled: !self.composer_editable(),
            empty_key: availability.unwrap_or("common.loading"),
        }
    }
}

#[derive(IntoElement)]
pub(crate) struct SelectionControl {
    kind: PickerKind,
    label: SharedString,
    icon: &'static str,
    color: gpui_kit::Hsla,
    provider: Option<String>,
    show_label: bool,
    direction: Direction,
    state: Entity<ComboboxState<PickerItems>>,
    previous_items: Rc<RefCell<Vec<PickerOption>>>,
    confirmed: Rc<RefCell<Option<PickValue>>>,
    items: Vec<PickerOption>,
    workspace: WeakEntity<Workspace>,
    disabled: bool,
    empty_key: &'static str,
}

pub(crate) struct PickerAppearance {
    pub label: SharedString,
    pub icon: &'static str,
    pub color: gpui_kit::Hsla,
    pub provider: Option<String>,
}

impl RenderOnce for SelectionControl {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if *self.previous_items.borrow() != self.items {
            let values: Vec<_> = self
                .items
                .iter()
                .filter(|item| item.current)
                .map(|item| item.value.clone())
                .collect();
            self.state.update(cx, |state, cx| {
                state.set_items(
                    PickerItems {
                        items: SearchableVec::new(self.items.clone()),
                        confirmed: self.confirmed.clone(),
                    },
                    window,
                    cx,
                );
                state.set_selected_values(&values, window, cx);
            });
            *self.previous_items.borrow_mut() = self.items;
        }
        let palette = cx.global::<Palette>().clone();
        let state = self.state.clone();
        let placeholder = locale::strings(cx).get(self.kind.search());
        let empty = locale::strings(cx).get(self.empty_key);
        let muted = palette.muted_foreground;
        div().key_context("ComposerPicker").h(px(32.0)).child(
            Combobox::new(&self.state)
                .appearance(false)
                .border_0()
                .p_0()
                .w_auto()
                .h(px(32.0))
                .disabled(self.disabled)
                .menu_width(px(320.0))
                .menu_max_h(px(320.0))
                .menu_offset(px(4.0))
                .menu_align(if self.direction == Direction::RightToLeft {
                    gpui_kit::base::Align::Start
                } else {
                    gpui_kit::base::Align::End
                })
                .menu_style(
                    StyleRefinement::default()
                        .border_0()
                        .rounded(px(12.0))
                        .bg(palette.elevated)
                        .shadow(elevation::floating(&palette)),
                )
                .search_clearable(false)
                .search_placeholder(placeholder)
                .empty(move |_, cx| {
                    div()
                        .px(px(12.0))
                        .py(px(24.0))
                        .text_center()
                        .text_color(muted)
                        .css_text(0.84375, 1.25, cx)
                        .child(empty.clone())
                })
                .check_icon(
                    gpui_kit::component::Icon::default().path(crate::assets::icon_path("check")),
                )
                .render_trigger(move |trigger, _, cx| {
                    let open = trigger.is_open();
                    let state = state.clone();
                    let disabled = self.disabled;
                    let leading = if let Some(provider) = self.provider.as_ref() {
                        self.workspace.upgrade().and_then(|workspace| {
                            workspace
                                .read(cx)
                                .render_provider_logo(provider, 17.0, 1.0, &palette)
                        })
                    } else {
                        Some(glyph(self.icon, 16.0, self.color).into_any_element())
                    };
                    div()
                        .id(self.kind.id())
                        .role(Role::Button)
                        .accessibility_id(self.kind.id())
                        .aria_label(self.label.clone())
                        .aria_expanded(open)
                        .on_a11y_action(gpui_kit::AccessibleAction::Click, move |_, window, cx| {
                            if disabled {
                                return;
                            }
                            state.update(cx, |state, cx| state.focus(window, cx));
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
                        .h(px(32.0))
                        .min_w_0()
                        .flex_row_from(self.direction)
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.7))
                        .children(leading)
                        .children(self.show_label.then(|| {
                            div()
                                .min_w_0()
                                .max_w(px(260.0))
                                .truncate()
                                .css_text(0.78125, 1.125, cx)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(self.color)
                                .child(self.label.clone())
                        }))
                        .test_support()
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send_catalog::{DirectoryCatalog, Load};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{ElementId, TestAppContext};
    use openchamber_native_core::send_config::{AgentEntry, CatalogModel, CatalogProvider};

    fn setup(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        workspace.connection = Some(
            crate::connection::Connection::new(crate::connection::ServerConfig {
                endpoint: openchamber_native_transport::endpoint::Endpoint::parse(
                    "http://localhost:1",
                )
                .expect("test endpoint"),
                password: None,
                client: Default::default(),
            })
            .expect("test network runtime"),
        );
        let directory = workspace.topology.projects[0].path.clone();
        let mut catalog = DirectoryCatalog::default();
        catalog.agents = Load::Ready(
            ["build", "plan"]
                .into_iter()
                .map(|name| AgentEntry {
                    name: name.into(),
                    display_name: None,
                    mode: Some(AgentMode::Primary),
                    hidden: false,
                    model: None,
                    color: None,
                })
                .collect(),
        );
        catalog.providers = Load::Ready(vec![CatalogProvider {
            id: "provider".into(),
            name: Some("Provider".into()),
            models: ["alpha", "beta"]
                .into_iter()
                .map(|id| CatalogModel {
                    id: id.into(),
                    name: Some(id.into()),
                    provider_model_id: None,
                    variants: vec!["low".into(), "high".into()],
                    context_limit: None,
                })
                .collect(),
        }]);
        catalog.opencode = Load::Ready(Default::default());
        workspace.send_catalog.settings =
            Some(openchamber_native_transport::client::ServerSettings {
                default_model: Some("provider/alpha#low".into()),
                default_agent: Some("build".into()),
                ..Default::default()
            });
        workspace
            .send_catalog
            .insert_test_directory(directory.clone(), catalog);
        workspace.choose_draft_project(&directory, window, cx);
    }

    #[gpui_kit::test]
    fn rendered_model_agent_effort_search_and_keyboard(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            window.activate_accessibility_for_test();
            workspace.update(cx, |workspace, cx| setup(workspace, window, cx));
            workspace.update(cx, |workspace, cx| {
                workspace.open_persistence(fixture.journal(), window, cx)
            });
        })
        .expect("open journal");
        crate::recovery_tests::ready(cx, &workspace);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(ElementId::Name("composer-model".into()), cx);
            window.render_frame(cx);
            window.press("down", cx);
            window.render_frame(cx);
            assert_search_focus(window, "Search models");
            window.input("beta", cx);
        })
        .expect("window opens");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .find(ElementId::Name(
                        "composer-model-option:provider/beta".into()
                    ))
                    .visible(),
                "search leaves the matching model"
            );
            window.press("enter", cx);
        })
        .expect("window opens");
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .send_selection()
                    .expect("selection")
                    .1
                    .model
                    .expect("model")
                    .model_id,
                "beta"
            )
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(ElementId::Name("composer-variant".into()), cx);
            window.render_frame(cx);
            window.press("ctrl-n", cx);
            window.press("ctrl-n", cx);
            window.press("enter", cx);
        })
        .expect("window opens");
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .send_selection()
                    .expect("selection")
                    .1
                    .model
                    .expect("model")
                    .variant
                    .as_deref(),
                Some("high")
            )
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(ElementId::Name("composer-agent".into()), cx);
            window.render_frame(cx);
            window.press("down", cx);
            window.render_frame(cx);
            assert_search_focus(window, "Search agents");
            window.input("plan", cx);
        })
        .expect("window opens");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .expect("window opens");
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .send_selection()
                    .expect("selection")
                    .1
                    .agent_name
                    .as_deref(),
                Some("plan")
            )
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(ElementId::Name("composer-model".into()), cx);
            window.render_frame(cx);
            window.press("up", cx);
            window.press("escape", cx);
        })
        .expect("window opens");
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .send_selection()
                    .expect("selection")
                    .1
                    .model
                    .expect("model")
                    .model_id,
                "beta",
                "Escape preserves the committed choice"
            )
        });
        crate::recovery_tests::close(cx, window, &workspace);
    }

    fn assert_search_focus(window: &Window, name: &str) {
        let tree = window
            .accessibility_tree_for_test()
            .expect("accessible picker");
        let (id, entry) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::TextInput && node.label() == Some(name))
            .expect("named search entry");
        assert_eq!(
            tree.focus, *id,
            "keyboard focus belongs to the named search editor"
        );
        let active = entry
            .active_descendant()
            .expect("highlighted option relation");
        assert!(
            tree.nodes.iter().any(|(id, node)| *id == active
                && node.role() == Role::ListItem
                && node.is_selected() == Some(true)),
            "the active descendant is the highlighted rendered row"
        );
    }

    #[gpui_kit::test]
    fn readonly_child_exposes_disabled_submit(cx: &mut TestAppContext) {
        use openchamber_native_core::ids::SessionId;
        use openchamber_native_core::session::SessionRecord;
        use openchamber_native_core::session_index::SessionChange;
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            window.activate_accessibility_for_test();
            workspace.update(cx, |workspace, cx| {
                let mut child = SessionRecord::new(SessionId::from("ses_readonly"), "/project");
                child.parent_id = Some(SessionId::from("ses_parent"));
                let _ = workspace.sessions.apply([SessionChange::Created(child)]);
                workspace.select_session(SessionId::from("ses_readonly"), window, cx);
                workspace.composer.update(cx, |composer, cx| {
                    composer.set_value("retained child draft", window, cx)
                });
            });
            window.draw(cx).clear(cx);
            let label = locale::strings(cx).get("chat.chatInput.actions.sendMessageAria");
            let tree = window
                .accessibility_tree_for_test()
                .expect("accessible tree");
            let node = tree
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == Role::Button && node.label() == Some(label.as_ref())
                })
                .expect("submit button");
            assert!(
                node.1.is_disabled(),
                "readonly submit exposes its disabled state"
            );
        })
        .expect("window opens");
    }
}
