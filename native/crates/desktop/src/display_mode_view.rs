//! The toolbar's Display mode menu (`SidebarHeader.tsx`): Select sessions,
//! the Sessions grouping and the project order as submenus that name the
//! current choice, then Collapse all and Expand all. Choosing a grouping or
//! an order keeps the menu open, as the web's view settings do, and saves
//! `sidebarGrouping` or `sidebarProjectSortOrder`; the list follows at once.
//! Grouped by time the list has no project headers, so the order is not
//! offered there.
//!
//! The sections manager, the project display choice, Detailed rows, Name
//! worktrees by branch and Mark all read belong to features the native
//! sidebar does not have yet and are not listed.

use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectLeft, SelectRight, SelectUp};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px,
};
use openchamber_native_core::ownership::{ConfiguredProject, SessionOwnershipIndex};
use openchamber_native_core::project_sort::{self, ProjectSortOrder, SortableProject};
use openchamber_native_core::sidebar_rows::{ActivityKey, SidebarViewMode};
use openchamber_native_transport::client::{ServerSettings, SettingsChanges};

use crate::activation::on_activate;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::session_menu::{MenuRow, menu_row};
use crate::sidebar_item::SidebarItem;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// A line of the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    SelectSessions,
    Grouping,
    SortProjects,
    CollapseAll,
    ExpandAll,
}

impl Line {
    /// How many choices the line's submenu offers; none for an action.
    fn choices(self) -> usize {
        match self {
            Self::Grouping => GROUPINGS.len(),
            Self::SortProjects => ProjectSortOrder::ALL.len(),
            Self::SelectSessions | Self::CollapseAll | Self::ExpandAll => 0,
        }
    }

    fn opens_submenu(self) -> bool {
        self.choices() > 0
    }

    /// Whether a divider precedes the line: it opens a group of the menu.
    fn opens_group(self) -> bool {
        matches!(self, Self::Grouping | Self::CollapseAll)
    }
}

/// The menu's lines. The project order steps aside while the list is grouped
/// by time.
fn lines(grouping: SidebarViewMode) -> Vec<Line> {
    let mut lines = vec![Line::SelectSessions, Line::Grouping];
    if grouping != SidebarViewMode::WorksByTime {
        lines.push(Line::SortProjects);
    }
    lines.extend([Line::CollapseAll, Line::ExpandAll]);
    lines
}

fn project_sort_label_key(order: ProjectSortOrder) -> &'static str {
    match order {
        ProjectSortOrder::Manual => "sessions.sidebar.header.projectSort.manual",
        ProjectSortOrder::AToZ => "sessions.sidebar.header.projectSort.aToZ",
        ProjectSortOrder::ZToA => "sessions.sidebar.header.projectSort.zToA",
        ProjectSortOrder::DateAdded => "sessions.sidebar.header.projectSort.dateAdded",
        ProjectSortOrder::Recent => "sessions.sidebar.header.projectSort.recent",
    }
}

/// The registered projects in the order the sidebar lists them
/// (`useProjectsInSidebarOrder`). Recent ranks by the newest session of each
/// project, as `ownership` resolves it; a `/btw` side conversation never
/// reaches a project (`partitionSidebarSessions`), so it ranks none.
pub(crate) fn projects_in_sidebar_order<'t>(
    projects: &'t [ConfiguredProject],
    settings: Option<&ServerSettings>,
    ownership: &SessionOwnershipIndex<'_>,
) -> Vec<&'t ConfiguredProject> {
    let order = settings
        .and_then(|settings| settings.project_sort)
        .unwrap_or_default();
    if order == ProjectSortOrder::Manual {
        return projects.iter().collect();
    }
    let sortable: Vec<SortableProject<'_>> = projects
        .iter()
        .map(|project| {
            let label = settings.and_then(|settings| {
                settings
                    .project_labels
                    .iter()
                    .find(|entry| entry.path == project.path)
                    .map(|entry| entry.label.as_str())
            });
            let times = settings.and_then(|settings| {
                settings
                    .project_times
                    .iter()
                    .find(|entry| entry.path == project.path)
            });
            let latest_activity = (order == ProjectSortOrder::Recent)
                .then(|| {
                    ownership
                        .project_sessions(&project.id)
                        .iter()
                        .filter(|session| !session.is_btw())
                        .map(|session| session.time.updated)
                        .max()
                })
                .flatten();
            SortableProject {
                label,
                path: &project.path,
                added_at: times.and_then(|times| times.added_at),
                last_opened_at: times.and_then(|times| times.last_opened_at),
                latest_activity,
            }
        })
        .collect();
    project_sort::sort_projects(&sortable, order)
        .into_iter()
        .map(|index| &projects[index])
        .collect()
}

const GROUPINGS: [SidebarViewMode; 2] = [SidebarViewMode::Works, SidebarViewMode::WorksByTime];

fn grouping_label_key(mode: SidebarViewMode) -> &'static str {
    match mode {
        SidebarViewMode::WorksByTime | SidebarViewMode::Timeline => {
            "sessions.sidebar.header.listGrouping.time"
        }
        SidebarViewMode::Works | SidebarViewMode::Projects => {
            "sessions.sidebar.header.listGrouping.project"
        }
    }
}

fn grouping_setting(mode: SidebarViewMode) -> &'static str {
    match mode {
        SidebarViewMode::WorksByTime | SidebarViewMode::Timeline => "time",
        SidebarViewMode::Works | SidebarViewMode::Projects => "project",
    }
}

/// The project order while saves are in flight.
///
/// The list follows the newest choice at once. Saves run one at a time, so an
/// older save can never land after a newer one; a choice made meanwhile is
/// saved next. A save that fails returns the list to the order the server
/// last confirmed, unless a newer choice is still waiting to be tried.
#[derive(Debug, Default)]
pub(crate) struct ProjectSortPreference {
    pending: Option<PendingProjectSort>,
}

#[derive(Debug)]
struct PendingProjectSort {
    /// What the server held when the first unconfirmed choice was made, then
    /// what each successful save left.
    confirmed: Option<ProjectSortOrder>,
    /// The newest choice.
    requested: ProjectSortOrder,
    /// The choice of the save in flight.
    saving: Option<ProjectSortOrder>,
}

/// What a recorded choice or a finished save asks of the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProjectSortStep {
    /// The value the settings show now: the newest unconfirmed choice, else
    /// what the server confirmed.
    pub(crate) shown: Option<ProjectSortOrder>,
    /// The order to save now, if any.
    pub(crate) save: Option<ProjectSortOrder>,
}

impl ProjectSortPreference {
    /// The choice the server has not confirmed, to keep over a settings read.
    pub(crate) fn requested(&self) -> Option<ProjectSortOrder> {
        self.pending.as_ref().map(|pending| pending.requested)
    }

    /// Records a choice over `stored`, the value the settings hold.
    pub(crate) fn choose(
        &mut self,
        order: ProjectSortOrder,
        stored: Option<ProjectSortOrder>,
    ) -> ProjectSortStep {
        match self.pending.as_mut() {
            Some(pending) => pending.requested = order,
            None => {
                self.pending = Some(PendingProjectSort {
                    confirmed: stored,
                    requested: order,
                    saving: None,
                });
            }
        }
        self.step()
    }

    /// Records a finished save: `Some` with the value the server now stores,
    /// or `None` when it failed. Returns nothing when no choice was waiting,
    /// so an answer that outlived its choice changes nothing.
    pub(crate) fn finish(
        &mut self,
        stored: Option<Option<ProjectSortOrder>>,
    ) -> Option<ProjectSortStep> {
        let pending = self.pending.as_mut()?;
        let attempted = pending.saving.take();
        if let Some(stored) = stored {
            pending.confirmed = stored;
        }
        // The newest choice was the one saved: the server's answer stands,
        // whatever it stored. A failure returns to what it last confirmed.
        // Only a choice newer than the attempt is still worth a save.
        if attempted == Some(pending.requested) {
            let confirmed = pending.confirmed;
            self.pending = None;
            return Some(ProjectSortStep {
                shown: confirmed,
                save: None,
            });
        }
        Some(self.step())
    }

    /// Starts the next save unless one is in flight or nothing differs.
    fn step(&mut self) -> ProjectSortStep {
        let Some(pending) = self.pending.as_mut() else {
            return ProjectSortStep {
                shown: None,
                save: None,
            };
        };
        if pending.saving.is_some() {
            return ProjectSortStep {
                shown: Some(pending.requested),
                save: None,
            };
        }
        if pending.requested == pending.confirmed.unwrap_or_default() {
            let confirmed = pending.confirmed;
            self.pending = None;
            return ProjectSortStep {
                shown: confirmed,
                save: None,
            };
        }
        pending.saving = Some(pending.requested);
        ProjectSortStep {
            shown: Some(pending.requested),
            save: Some(pending.requested),
        }
    }
}

/// The open menu.
pub(crate) struct DisplayMenu {
    highlighted: Option<usize>,
    /// The line whose submenu is open, with its highlighted choice when the
    /// keyboard is in it.
    submenu: Option<(Line, Option<usize>)>,
    focus: gpui_kit::FocusHandle,
}

impl Workspace {
    fn sidebar_grouping(&self) -> SidebarViewMode {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.sidebar_view)
            .unwrap_or_default()
    }

    /// The toolbar's Display mode button.
    pub(crate) fn toggle_display_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.display_menu.take().is_some() {
            cx.notify();
            return;
        }
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.session_management.display_menu = Some(DisplayMenu {
            highlighted: None,
            submenu: None,
            focus,
        });
        cx.notify();
    }

    /// Lists sessions by project or by time. The choice shows at once and is
    /// saved for every window of this server.
    fn choose_grouping(
        &mut self,
        mode: SidebarViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_grouping() == mode {
            return;
        }
        if let Some(settings) = self.send_catalog.settings.as_mut() {
            settings.sidebar_view = Some(mode);
        }
        self.rebuild_sidebar();
        let grouping = grouping_setting(mode);
        self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    client
                        .save_settings(&SettingsChanges {
                            sidebar_grouping: Some(grouping),
                            ..SettingsChanges::default()
                        })
                        .await
                })
            },
            |_, result, _, _| {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "the sidebar grouping could not be saved");
                }
            },
            window,
            cx,
        );
        cx.notify();
    }

    fn sidebar_project_sort(&self) -> ProjectSortOrder {
        self.session_management
            .project_sort
            .requested()
            .or_else(|| {
                self.send_catalog
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.project_sort)
            })
            .unwrap_or_default()
    }

    /// Orders the projects. The choice shows at once and is saved for every
    /// window of this server; see [`ProjectSortPreference`].
    fn choose_project_sort(
        &mut self,
        order: ProjectSortOrder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_project_sort() == order {
            return;
        }
        let stored = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.project_sort);
        let step = self.session_management.project_sort.choose(order, stored);
        self.apply_project_sort_step(step, window, cx);
    }

    /// Shows what `step` says and starts the save it asks for.
    fn apply_project_sort_step(
        &mut self,
        step: ProjectSortStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.send_catalog.settings.as_mut().is_some_and(|settings| {
            let changed = settings.project_sort != step.shown;
            settings.project_sort = step.shown;
            changed
        });
        if changed {
            self.rebuild_sidebar();
        }
        cx.notify();
        let Some(order) = step.save else {
            return;
        };
        let runtime = self.runtime.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    client
                        .save_settings(&SettingsChanges {
                            sidebar_project_sort_order: Some(order.as_setting()),
                            ..SettingsChanges::default()
                        })
                        .await
                })
            },
            move |this, result, window, cx| {
                let stored = match result {
                    Some(Ok(settings)) => Some(settings.project_sort),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the project order could not be saved");
                        None
                    }
                    None => None,
                };
                this.finish_project_sort_save(&runtime, stored, window, cx);
            },
            window,
            cx,
        );
        if !started {
            // The built-in sample workspace has no server to disagree, so the
            // choice stands there; a real server that cannot be reached now
            // stored nothing.
            let stored = self.is_synthetic().then_some(Some(order));
            let runtime = self.runtime.clone();
            self.finish_project_sort_save(&runtime, stored, window, cx);
        }
    }

    /// A project order save finished for `runtime`: `Some` with what the
    /// server stores now, or `None` when the save failed. A save of another
    /// server changes nothing here.
    pub(crate) fn finish_project_sort_save(
        &mut self,
        runtime: &openchamber_native_core::ids::RuntimeId,
        stored: Option<Option<ProjectSortOrder>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *runtime != self.runtime {
            return;
        }
        if let Some(step) = self.session_management.project_sort.finish(stored) {
            self.apply_project_sort_step(step, window, cx);
        }
    }

    /// Collapse all: every zone and project folds, and every worktree
    /// closes its sessions.
    pub(crate) fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let projects: Vec<_> = self
            .topology
            .projects
            .iter()
            .map(|project| project.id.clone())
            .collect();
        let state = &mut self.sidebar_state;
        state.collapsed_groups.clear();
        state.expanded_works.clear();
        state.collapsed_activities.extend([
            ActivityKey::Chats,
            ActivityKey::Pinned,
            ActivityKey::Work,
            ActivityKey::Timeline,
        ]);
        state.collapsed_projects.extend(projects);
        self.rebuild_sidebar();
        cx.notify();
    }

    /// Expand all: every zone and project opens, and every worktree lists
    /// its sessions.
    pub(crate) fn expand_all(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.sidebar_state;
        state.collapsed_groups.clear();
        state.collapsed_activities.clear();
        state.collapsed_projects.clear();
        self.rebuild_sidebar();
        let works: Vec<String> = self
            .sidebar_items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Worktree { group_key, .. } => Some(group_key.clone()),
                _ => None,
            })
            .collect();
        self.sidebar_state.expanded_works.extend(works);
        self.rebuild_sidebar();
        cx.notify();
    }

    fn run_display_line(&mut self, line: Line, keyboard: bool, cx: &mut Context<Self>) {
        match line {
            Line::Grouping | Line::SortProjects => {
                let position = lines(self.sidebar_grouping())
                    .iter()
                    .position(|listed| *listed == line);
                if let Some(menu) = self.session_management.display_menu.as_mut() {
                    menu.highlighted = position;
                    menu.submenu = Some((line, keyboard.then_some(0)));
                }
                cx.notify();
            }
            Line::SelectSessions => {
                self.session_management.display_menu = None;
                self.toggle_selection_mode(cx);
            }
            Line::CollapseAll => {
                self.session_management.display_menu = None;
                self.collapse_all(cx);
            }
            Line::ExpandAll => {
                self.session_management.display_menu = None;
                self.expand_all(cx);
            }
        }
    }

    fn step_display_menu(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = lines(self.sidebar_grouping()).len();
        let Some(menu) = self.session_management.display_menu.as_mut() else {
            return;
        };
        let wrap = |from: Option<usize>, count: usize| {
            openchamber_native_core::session_actions::step_highlight_by(count, |_| true, from, step)
        };
        match &mut menu.submenu {
            Some((line, choice @ Some(_))) => *choice = wrap(*choice, line.choices()),
            _ => {
                menu.submenu = None;
                menu.highlighted = wrap(menu.highlighted, count);
            }
        }
        cx.notify();
    }

    fn confirm_display_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listed = lines(self.sidebar_grouping());
        let Some(menu) = self.session_management.display_menu.as_ref() else {
            return;
        };
        if let Some((line, Some(choice))) = menu.submenu {
            self.choose_display_choice(line, choice, window, cx);
            return;
        }
        if let Some(line) = menu
            .highlighted
            .and_then(|index| listed.get(index).copied())
        {
            self.run_display_line(line, true, cx);
        }
    }

    /// Applies choice `index` of `line`'s submenu.
    fn choose_display_choice(
        &mut self,
        line: Line,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match line {
            Line::Grouping => {
                if let Some(mode) = GROUPINGS.get(index) {
                    self.choose_grouping(*mode, window, cx);
                }
            }
            Line::SortProjects => {
                if let Some(order) = ProjectSortOrder::ALL.get(index) {
                    self.choose_project_sort(*order, window, cx);
                }
            }
            Line::SelectSessions | Line::CollapseAll | Line::ExpandAll => {}
        }
    }

    fn horizontal_display_menu(&mut self, toward_end: bool, cx: &mut Context<Self>) {
        let listed = lines(self.sidebar_grouping());
        let Some(menu) = self.session_management.display_menu.as_mut() else {
            return;
        };
        let line = menu
            .highlighted
            .and_then(|index| listed.get(index).copied());
        match line {
            Some(line) if toward_end && line.opens_submenu() => {
                menu.submenu = Some((line, Some(0)));
            }
            _ if !toward_end => menu.submenu = None,
            _ => {}
        }
        cx.notify();
    }

    /// The open menu, below the Display mode button and aligned to its
    /// start (`align="start"`).
    pub(crate) fn render_display_menu(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.session_management.display_menu.as_ref()?;
        let strings = locale::strings(cx).clone();
        let current = self.sidebar_grouping();
        let sort = self.sidebar_project_sort();
        let selecting = self.session_management.selection.is_enabled();
        let open_submenu = menu.submenu.map(|(line, _)| line);
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, line) in lines(current).into_iter().enumerate() {
            let (id, label, icon, hint) = match line {
                Line::SelectSessions => (
                    "select-sessions",
                    strings.get(if selecting {
                        "sessions.sidebar.header.actions.exitSelection"
                    } else {
                        "sessions.sidebar.header.actions.selectSessions"
                    }),
                    "checkbox-multiple",
                    None,
                ),
                Line::Grouping => (
                    "grouping",
                    strings.get("sessions.sidebar.header.listGrouping.label"),
                    if current == SidebarViewMode::WorksByTime {
                        "time"
                    } else {
                        "folders"
                    },
                    // A submenu names its current choice.
                    Some(strings.get(grouping_label_key(current))),
                ),
                Line::SortProjects => (
                    "sort-projects",
                    strings.get("sessions.sidebar.header.actions.sortProjects"),
                    "arrow-up-down",
                    Some(strings.get(project_sort_label_key(sort))),
                ),
                Line::CollapseAll => (
                    "collapse-all",
                    strings.get("sessions.sidebar.header.displayMode.collapseAll"),
                    "contract-up-down",
                    None,
                ),
                Line::ExpandAll => (
                    "expand-all",
                    strings.get("sessions.sidebar.header.displayMode.expandAll"),
                    "expand-up-down",
                    None,
                ),
            };
            let row = menu_row(
                MenuRow {
                    id: format!("display-menu-item:{id}"),
                    label,
                    hint,
                    icon: Some(icon),
                    enabled: true,
                    highlighted: menu.highlighted == Some(index),
                    destructive: false,
                    checked: None,
                    opens_submenu: line.opens_submenu(),
                },
                palette,
                direction,
                cx,
            )
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if let Some(menu) = this.session_management.display_menu.as_mut()
                    && menu.highlighted != Some(index)
                {
                    menu.highlighted = Some(index);
                    menu.submenu = line.opens_submenu().then_some((line, None));
                    cx.notify();
                }
            }));
            let row = on_activate(row, cx, move |this, _, cx| {
                this.run_display_line(line, false, cx)
            })
            .test_support();
            let submenu = (open_submenu == Some(line))
                .then(|| self.render_choice_submenu(menu, line, palette, direction, cx));
            if line.opens_group() {
                rows.push(
                    div()
                        .mx(px(-4.0))
                        .my(px(2.0))
                        .h(px(1.0))
                        .bg(palette.border)
                        .into_any_element(),
                );
            }
            rows.push(
                div()
                    .relative()
                    .child(row)
                    .children(submenu)
                    .into_any_element(),
            );
        }
        let popup = div()
            .id("display-menu")
            .role(Role::Menu)
            .accessibility_id("display-menu")
            .aria_label(strings.get("sessions.sidebar.header.displayMode.label"))
            .track_focus(&menu.focus)
            .key_context(crate::session_menu::MENU_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_display_menu(1, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_display_menu(-1, cx)))
            .on_action(cx.listener(move |this, _: &SelectRight, _, cx| {
                this.horizontal_display_menu(direction == Direction::LeftToRight, cx)
            }))
            .on_action(cx.listener(move |this, _: &SelectLeft, _, cx| {
                this.horizontal_display_menu(direction == Direction::RightToLeft, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Confirm, window, cx| this.confirm_display_menu(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                this.session_management.display_menu = None;
                cx.notify();
            }))
            .on_mouse_down_out(
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, _, cx| {
                    let inside = this
                        .session_management
                        .submenu_bounds
                        .is_some_and(|bounds| bounds.contains(&event.position));
                    if !inside {
                        this.session_management.display_menu = None;
                        cx.notify();
                    }
                }),
            )
            .occlude()
            .min_w(px(200.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(crate::elevation::floating(palette))
            .flex()
            .flex_col()
            .children(rows)
            .test_support();
        let anchor = match direction {
            Direction::LeftToRight => gpui_kit::Anchor::TopLeft,
            Direction::RightToLeft => gpui_kit::Anchor::TopRight,
        };
        Some(
            div()
                .absolute()
                .top(px(24.0))
                .inset_start(direction, px(0.0))
                .size_0()
                .child(
                    gpui_kit::deferred(
                        gpui_kit::anchored()
                            .anchor(anchor)
                            .snap_to_window_with_margin(gpui_kit::Edges::all(px(8.0)))
                            .child(div().mt(px(4.0)).child(popup)),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    /// The choices of `line`, beside it, with a check on the current one.
    fn render_choice_submenu(
        &self,
        menu: &DisplayMenu,
        line: Line,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        // The element ID, the label and whether it is the current choice.
        let (panel_id, options): (&'static str, Vec<(String, &'static str, bool)>) = match line {
            Line::SortProjects => {
                let current = self.sidebar_project_sort();
                (
                    "display-menu-sort",
                    ProjectSortOrder::ALL
                        .into_iter()
                        .map(|order| {
                            (
                                format!("display-menu-sort:{}", order.as_setting()),
                                project_sort_label_key(order),
                                current == order,
                            )
                        })
                        .collect(),
                )
            }
            Line::Grouping | Line::SelectSessions | Line::CollapseAll | Line::ExpandAll => {
                let current = self.sidebar_grouping();
                (
                    "display-menu-grouping",
                    GROUPINGS
                        .into_iter()
                        .map(|mode| {
                            (
                                format!("display-menu-grouping:{}", grouping_setting(mode)),
                                grouping_label_key(mode),
                                current == mode,
                            )
                        })
                        .collect(),
                )
            }
        };
        let highlighted = match menu.submenu {
            Some((open, choice)) if open == line => choice,
            _ => None,
        };
        let choices = options
            .into_iter()
            .enumerate()
            .map(|(index, (id, label_key, checked))| {
                let row = menu_row(
                    MenuRow {
                        id,
                        label: strings.get(label_key),
                        hint: None,
                        icon: None,
                        enabled: true,
                        highlighted: highlighted == Some(index),
                        destructive: false,
                        checked: Some(checked),
                        opens_submenu: false,
                    },
                    palette,
                    direction,
                    cx,
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(menu) = this.session_management.display_menu.as_mut()
                        && menu.submenu != Some((line, Some(index)))
                    {
                        menu.submenu = Some((line, Some(index)));
                        cx.notify();
                    }
                }));
                on_activate(row, cx, move |this, window, cx| {
                    this.choose_display_choice(line, index, window, cx)
                })
                .test_support()
                .into_any_element()
            })
            .collect::<Vec<_>>();
        let workspace = cx.entity().downgrade();
        let panel = div()
            .id(panel_id)
            .role(Role::Menu)
            .accessibility_id(panel_id)
            .occlude()
            .min_w(px(160.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(crate::elevation::floating(palette))
            .flex()
            .flex_col()
            .children(choices)
            .child(
                gpui_kit::canvas(
                    move |bounds, _, cx| {
                        let _ = workspace.update(cx, |this, _| {
                            this.session_management.submenu_bounds = Some(bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .test_support();
        let anchor = match direction {
            Direction::LeftToRight => gpui_kit::Anchor::TopLeft,
            Direction::RightToLeft => gpui_kit::Anchor::TopRight,
        };
        div()
            .absolute()
            .top(px(-4.0))
            .inset_end(direction, px(0.0))
            .size_0()
            .child(
                gpui_kit::deferred(
                    gpui_kit::anchored()
                        .anchor(anchor)
                        .snap_to_window_with_margin(gpui_kit::Edges::all(px(8.0)))
                        .child(div().margin_start(direction, px(8.0)).child(panel)),
                )
                .with_priority(2),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AppContext;
    use openchamber_native_core::ids::ProjectId;
    use openchamber_native_core::ownership::OwnershipInputs;
    use openchamber_native_core::session::SessionRecord;
    use openchamber_native_transport::client::{ProjectLabel, ProjectTimes};

    #[test]
    fn the_grouping_saves_the_web_setting_values() {
        assert_eq!(grouping_setting(SidebarViewMode::Works), "project");
        assert_eq!(grouping_setting(SidebarViewMode::WorksByTime), "time");
        assert_eq!(
            grouping_label_key(SidebarViewMode::WorksByTime),
            "sessions.sidebar.header.listGrouping.time"
        );
    }

    #[test]
    fn the_project_order_is_offered_only_under_project_headers() {
        assert_eq!(
            lines(SidebarViewMode::Works),
            [
                Line::SelectSessions,
                Line::Grouping,
                Line::SortProjects,
                Line::CollapseAll,
                Line::ExpandAll,
            ]
        );
        assert_eq!(
            lines(SidebarViewMode::WorksByTime),
            [
                Line::SelectSessions,
                Line::Grouping,
                Line::CollapseAll,
                Line::ExpandAll,
            ]
        );
        assert_eq!(Line::SortProjects.choices(), 5);
        assert!(!Line::SelectSessions.opens_submenu());
    }

    #[gpui_kit::test]
    fn the_menu_orders_projects_and_starts_the_selection_mode(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{ElementId, SharedString};

        let name = |id: &str| ElementId::Name(SharedString::from(id.to_owned()));
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |state, _| {
                state.send_catalog.settings = Some(ServerSettings::default());
            });
            window.render_frame(cx);
            window.click(name("display-mode"), cx);
            window.render_frame(cx);
            window.hover(name("display-menu-item:sort-projects"), cx);
            window.render_frame(cx);
            window.click(name("display-menu-sort:a-z"), cx);
            window.render_frame(cx);
            assert!(
                window.try_find(name("display-menu")).is_some(),
                "a view setting keeps the menu open"
            );
            let state = workspace.read(cx);
            assert_eq!(state.sidebar_project_sort(), ProjectSortOrder::AToZ);
            let listed: Vec<ProjectId> = state
                .sidebar_items
                .iter()
                .filter_map(|item| match item {
                    SidebarItem::Project { project_id, .. } => Some(project_id.clone()),
                    _ => None,
                })
                .collect();
            // No project carries a label here, so each sorts by its path.
            let mut projects: Vec<&ConfiguredProject> = state.topology.projects.iter().collect();
            projects.sort_by_key(|project| project.path.to_lowercase());
            let expected: Vec<ProjectId> = projects
                .into_iter()
                .map(|project| project.id.clone())
                .filter(|id| listed.contains(id))
                .collect();
            assert!(listed.len() > 1, "the fixture lists several projects");
            assert_eq!(listed, expected, "the headers follow the order at once");
        })
        .expect("window is open");
        cx.update_window(window, |_, window, cx| {
            window.click(name("display-menu-item:select-sessions"), cx);
            window.render_frame(cx);
            assert!(
                window.try_find(name("display-menu")).is_none(),
                "an action closes the menu"
            );
            assert!(workspace.read(cx).session_management.selection.is_enabled());
            assert!(window.try_find(name("bulk-action-bar")).is_some());
        })
        .expect("window is open");
    }

    fn step(shown: Option<ProjectSortOrder>, save: Option<ProjectSortOrder>) -> ProjectSortStep {
        ProjectSortStep { shown, save }
    }

    #[test]
    fn a_failed_save_returns_to_the_confirmed_order() {
        use ProjectSortOrder::{AToZ, Recent};
        let mut preference = ProjectSortPreference::default();
        assert_eq!(
            preference.choose(AToZ, Some(Recent)),
            step(Some(AToZ), Some(AToZ))
        );
        assert_eq!(preference.requested(), Some(AToZ));
        assert_eq!(preference.finish(None), Some(step(Some(Recent), None)));
        assert_eq!(preference.requested(), None, "nothing stays unconfirmed");
        // Nothing was stored before: the failure returns to no choice.
        assert_eq!(preference.choose(AToZ, None), step(Some(AToZ), Some(AToZ)));
        assert_eq!(preference.finish(None), Some(step(None, None)));
    }

    #[test]
    fn saves_run_one_at_a_time_and_the_newest_choice_wins() {
        use ProjectSortOrder::{AToZ, DateAdded, Recent, ZToA};
        let mut preference = ProjectSortPreference::default();
        assert_eq!(preference.choose(AToZ, None), step(Some(AToZ), Some(AToZ)));
        // Two more choices while the first save is in flight start no save.
        assert_eq!(preference.choose(ZToA, Some(AToZ)), step(Some(ZToA), None));
        assert_eq!(
            preference.choose(Recent, Some(ZToA)),
            step(Some(Recent), None)
        );
        // The first save lands: only the newest choice is saved next.
        assert_eq!(
            preference.finish(Some(Some(AToZ))),
            Some(step(Some(Recent), Some(Recent)))
        );
        assert_eq!(
            preference.finish(Some(Some(Recent))),
            Some(step(Some(Recent), None))
        );
        assert_eq!(preference.requested(), None);
        assert_eq!(
            preference.finish(Some(Some(DateAdded))),
            None,
            "an answer with no choice waiting changes nothing"
        );
    }

    #[test]
    fn a_newer_choice_survives_an_older_save_that_failed() {
        use ProjectSortOrder::{AToZ, Manual, ZToA};
        let mut preference = ProjectSortPreference::default();
        let _ = preference.choose(AToZ, Some(ZToA));
        let _ = preference.choose(Manual, Some(AToZ));
        // The older save fails; the newer choice is tried on its own.
        assert_eq!(
            preference.finish(None),
            Some(step(Some(Manual), Some(Manual)))
        );
        // It fails too: back to what the server confirmed before either.
        assert_eq!(preference.finish(None), Some(step(Some(ZToA), None)));
        // Choosing the confirmed order again while a save is in flight
        // needs no second save once that save fails.
        let _ = preference.choose(AToZ, Some(ZToA));
        let _ = preference.choose(ZToA, Some(AToZ));
        assert_eq!(preference.finish(None), Some(step(Some(ZToA), None)));
        assert_eq!(preference.requested(), None);
    }

    #[test]
    fn a_save_the_server_did_not_keep_is_not_repeated() {
        use ProjectSortOrder::AToZ;
        let mut preference = ProjectSortPreference::default();
        let _ = preference.choose(AToZ, None);
        assert_eq!(
            preference.finish(Some(None)),
            Some(step(None, None)),
            "the server's answer stands"
        );
    }

    #[gpui_kit::test]
    fn a_failed_project_order_save_puts_the_list_back(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt;

        let (window, workspace) = crate::workspace_tests::open(cx);
        let headers = |cx: &mut gpui_kit::TestAppContext| -> Vec<ProjectId> {
            cx.read(|cx| {
                workspace
                    .read(cx)
                    .sidebar_items
                    .iter()
                    .filter_map(|item| match item {
                        SidebarItem::Project { project_id, .. } => Some(project_id.clone()),
                        _ => None,
                    })
                    .collect()
            })
        };
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |state, _| {
                state.send_catalog.settings = Some(ServerSettings::default());
                state.rebuild_sidebar();
            });
            window.render_frame(cx);
        })
        .expect("window is open");
        let registered = headers(cx);
        assert!(registered.len() > 1, "the fixture lists several projects");
        let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |state, cx| {
                // A save is in flight for Z to A; the list already follows it.
                let step = state
                    .session_management
                    .project_sort
                    .choose(ProjectSortOrder::ZToA, None);
                assert_eq!(step.save, Some(ProjectSortOrder::ZToA));
                if let Some(settings) = state.send_catalog.settings.as_mut() {
                    settings.project_sort = step.shown;
                }
                state.rebuild_sidebar();
                assert_eq!(state.sidebar_project_sort(), ProjectSortOrder::ZToA);
                // A settings read lands meanwhile: the choice stays on top.
                let mut read = ServerSettings::default();
                state.overlay_unsettled_settings(&mut read, cx);
                assert_eq!(read.project_sort, Some(ProjectSortOrder::ZToA));
                // Another server's answer changes nothing.
                state.finish_project_sort_save(
                    &openchamber_native_core::ids::RuntimeId::from("another-server"),
                    None,
                    window,
                    cx,
                );
                assert_eq!(state.sidebar_project_sort(), ProjectSortOrder::ZToA);
                // The save fails.
                state.finish_project_sort_save(&runtime, None, window, cx);
                assert_eq!(state.sidebar_project_sort(), ProjectSortOrder::Manual);
                assert_eq!(
                    state
                        .send_catalog
                        .settings
                        .as_ref()
                        .and_then(|settings| settings.project_sort),
                    None,
                    "the stored value is what the server last confirmed"
                );
            });
            window.render_frame(cx);
        })
        .expect("window is open");
        assert_eq!(
            headers(cx),
            registered,
            "the list is rebuilt in the prior order"
        );
    }

    #[test]
    fn a_side_conversation_does_not_rank_its_project_as_recent() {
        use openchamber_native_core::session::SessionKind;
        let projects = [
            ConfiguredProject {
                id: ProjectId::from("one"),
                path: "/work/one".to_owned(),
            },
            ConfiguredProject {
                id: ProjectId::from("two"),
                path: "/work/two".to_owned(),
            },
        ];
        let mut ordinary = SessionRecord::new("ordinary", "/work/two");
        ordinary.time.updated = 10;
        let mut side = SessionRecord::new("side", "/work/one");
        side.time.updated = 99;
        side.kind = SessionKind::Btw;
        let sessions = [ordinary, side];
        let inputs = OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            ..OwnershipInputs::default()
        };
        let ownership = SessionOwnershipIndex::build(&inputs);
        let settings = ServerSettings {
            project_sort: Some(ProjectSortOrder::Recent),
            ..ServerSettings::default()
        };
        assert_eq!(
            paths(&projects_in_sidebar_order(
                &projects,
                Some(&settings),
                &ownership
            )),
            ["/work/two", "/work/one"],
            "only the ordinary session counts as activity"
        );
    }

    fn paths<'t>(projects: &[&'t ConfiguredProject]) -> Vec<&'t str> {
        projects
            .iter()
            .map(|project| project.path.as_str())
            .collect()
    }

    #[test]
    fn projects_follow_the_saved_order_with_labels_times_and_sessions() {
        let projects = [
            ConfiguredProject {
                id: ProjectId::from("one"),
                path: "/work/zeta".to_owned(),
            },
            ConfiguredProject {
                id: ProjectId::from("two"),
                path: "/work/beta".to_owned(),
            },
            ConfiguredProject {
                id: ProjectId::from("three"),
                path: "/work/alpha".to_owned(),
            },
        ];
        let mut old = SessionRecord::new("old", "/work/zeta");
        old.time.updated = 10;
        let mut new = SessionRecord::new("new", "/work/beta");
        new.time.updated = 90;
        let sessions = [old, new];
        let inputs = OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            ..OwnershipInputs::default()
        };
        let ownership = SessionOwnershipIndex::build(&inputs);
        let mut settings = ServerSettings {
            project_labels: vec![ProjectLabel {
                path: "/work/zeta".to_owned(),
                label: "Aardvark".to_owned(),
            }],
            project_times: vec![
                ProjectTimes {
                    path: "/work/alpha".to_owned(),
                    added_at: Some(30),
                    last_opened_at: Some(7),
                },
                ProjectTimes {
                    path: "/work/zeta".to_owned(),
                    added_at: Some(20),
                    last_opened_at: None,
                },
            ],
            ..ServerSettings::default()
        };
        let ordered = |settings: &ServerSettings| {
            paths(&projects_in_sidebar_order(
                &projects,
                Some(settings),
                &ownership,
            ))
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            ordered(&settings),
            ["/work/zeta", "/work/beta", "/work/alpha"],
            "no saved order keeps the registered one"
        );
        settings.project_sort = Some(ProjectSortOrder::AToZ);
        assert_eq!(
            ordered(&settings),
            ["/work/alpha", "/work/beta", "/work/zeta"],
            "a label sorts in place of its path: /work/alpha, /work/beta, aardvark"
        );
        settings.project_sort = Some(ProjectSortOrder::DateAdded);
        assert_eq!(
            ordered(&settings),
            ["/work/alpha", "/work/zeta", "/work/beta"]
        );
        settings.project_sort = Some(ProjectSortOrder::Recent);
        assert_eq!(
            ordered(&settings),
            ["/work/beta", "/work/zeta", "/work/alpha"],
            "the newest session ranks its project first"
        );
        assert_eq!(
            paths(&projects_in_sidebar_order(&projects, None, &ownership)),
            ["/work/zeta", "/work/beta", "/work/alpha"]
        );
    }
}
