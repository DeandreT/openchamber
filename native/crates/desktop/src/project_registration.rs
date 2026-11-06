//! Registering, renaming and closing projects.
//!
//! Follows `useProjectsStore.ts`: the settings document's `projects` array
//! is the registry, written whole with `activeProjectId`. Each change reads
//! the stored entries first and keeps the fields this client does not
//! edit. Add project browses the server's folders, since the server can be
//! another machine (`DirectoryExplorerDialog`); the typed path is the
//! project, and the list under it opens subfolders. Cloning a repository
//! and importing OpenCode's projects are not part of this dialog yet.

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window,
    div, prelude::FluentBuilder, px,
};
use openchamber_native_core::file_tree::{DirectoryEntry, EntryKind};
use openchamber_native_core::folders::FolderChange;
use openchamber_native_core::ids::{ProjectId, SessionId};
use openchamber_native_core::path;
use openchamber_native_transport::projects::ProjectEntries;
use serde_json::{Map, Value, json};

use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::session_export::dialog_button;
use crate::theme::Palette;
use crate::workspace::Workspace;

const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;

/// `COLOR_PRESETS`: the colours a new project takes in turn.
const COLOR_PRESETS: [&str; 8] = [
    "#e5484d", "#f76b15", "#d6a312", "#30a46c", "#12a594", "#3e63dd", "#8e4ec6", "#d6409f",
];

/// `deriveProjectLabel`: the folder's name, `Root` for `/`.
pub(crate) fn derive_project_label(path: &str) -> String {
    match path::normalize(path) {
        None => "Root".to_owned(),
        Some(normalized) if normalized == "/" => "Root".to_owned(),
        Some(normalized) => normalized
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or(&normalized)
            .to_owned(),
    }
}

/// `pickAutoColor`: one of the least used presets; `pick` chooses among
/// them.
fn pick_auto_color(
    entries: &[Map<String, Value>],
    pick: impl FnOnce(usize) -> usize,
) -> &'static str {
    let counts: Vec<usize> = COLOR_PRESETS
        .iter()
        .map(|color| {
            entries
                .iter()
                .filter(|entry| entry.get("color").and_then(Value::as_str) == Some(color))
                .count()
        })
        .collect();
    let least = counts.iter().copied().min().unwrap_or_default();
    let candidates: Vec<&str> = COLOR_PRESETS
        .iter()
        .zip(&counts)
        .filter(|(_, count)| **count == least)
        .map(|(color, _)| *color)
        .collect();
    candidates[pick(candidates.len()) % candidates.len()]
}

/// The registry after adding `path`: an existing project becomes active; a
/// new one is appended with an automatic label and colour and made active.
fn with_project(
    mut registry: ProjectEntries,
    path: &str,
    now: i64,
    pick: impl FnOnce(usize) -> usize,
) -> Option<(ProjectEntries, String)> {
    let id = ProjectId::from_path(path)?.as_str().to_owned();
    if let Some(existing) = registry
        .entries
        .iter()
        .find(|entry| ProjectEntries::path(entry) == Some(path))
        .and_then(|entry| ProjectEntries::id(entry))
        .map(str::to_owned)
    {
        registry.active_project_id = Some(existing.clone());
        return Some((registry, existing));
    }
    let color = pick_auto_color(&registry.entries, pick);
    let entry = json!({
        "id": id,
        "path": path,
        "label": derive_project_label(path),
        "labelSource": "auto",
        "color": color,
        "addedAt": now,
        "lastOpenedAt": now,
    });
    if let Value::Object(entry) = entry {
        registry.entries.push(entry);
    }
    registry.active_project_id = Some(id.clone());
    Some((registry, id))
}

/// The registry without project `id`; the first remaining one becomes
/// active when it was.
fn without_project(mut registry: ProjectEntries, id: &str) -> ProjectEntries {
    registry
        .entries
        .retain(|entry| ProjectEntries::id(entry) != Some(id));
    if registry.active_project_id.as_deref() == Some(id) {
        registry.active_project_id = registry
            .entries
            .first()
            .and_then(|entry| ProjectEntries::id(entry))
            .map(str::to_owned);
    }
    registry
}

/// The registry with project `id` labelled `label` by the user; an empty
/// label changes nothing.
fn with_label(mut registry: ProjectEntries, id: &str, label: &str) -> ProjectEntries {
    let label = label.trim();
    if label.is_empty() {
        return registry;
    }
    for entry in &mut registry.entries {
        if ProjectEntries::id(entry) == Some(id) {
            entry.insert("label".to_owned(), json!(label));
            entry.insert("labelSource".to_owned(), json!("manual"));
        }
    }
    registry
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn random_index(bound: usize) -> usize {
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i64(now_ms());
    (hasher.finish() % bound.max(1) as u64) as usize
}

/// The open Add project dialog.
pub(crate) struct AddProject {
    input: Entity<InputState>,
    /// The folder the list shows and what its read found.
    listed: Option<String>,
    entries: Result<Vec<DirectoryEntry>, ()>,
    /// A session to move into the project once it is registered.
    move_after: Option<SessionId>,
    adding: bool,
    _subscription: Subscription,
    _read: Option<Task<()>>,
}

/// An item of a project header's menu (`renderProjectMenuItems`). New
/// session, worktree and space management, the source board, Reveal and
/// Archive all are not part of the native menu yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectMenuItem {
    Edit,
    Close,
}

impl ProjectMenuItem {
    const ALL: [Self; 2] = [Self::Edit, Self::Close];

    fn id(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Close => "close",
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Self::Edit => "sessions.sidebar.project.actions.edit",
            Self::Close => "sessions.sidebar.project.actions.closeProject",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Edit => "pencil-ai",
            Self::Close => "close",
        }
    }
}

/// An open project header menu.
pub(crate) struct ProjectMenu {
    pub(crate) project: ProjectId,
    position: gpui_kit::Point<gpui_kit::Pixels>,
    highlighted: Option<usize>,
    focus: gpui_kit::FocusHandle,
}

/// The open Edit project dialog.
pub(crate) struct EditProject {
    project: String,
    input: Entity<InputState>,
}

impl Workspace {
    /// A project's label from settings, else its folder's name.
    pub(crate) fn project_display_label(&self, project_path: &str) -> SharedString {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| {
                settings
                    .project_labels
                    .iter()
                    .find(|entry| entry.path == project_path)
            })
            .map(|entry| SharedString::from(entry.label.clone()))
            .unwrap_or_else(|| derive_project_label(project_path).into())
    }

    /// Opens a project header's menu at the pointer.
    pub(crate) fn open_project_menu(
        &mut self,
        project: ProjectId,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.session_management.project_menu = Some(ProjectMenu {
            project,
            position,
            highlighted: None,
            focus,
        });
        cx.notify();
    }

    fn run_project_menu_item(
        &mut self,
        item: ProjectMenuItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.session_management.project_menu.take() else {
            return;
        };
        match item {
            ProjectMenuItem::Edit => self.open_edit_project(&menu.project, window, cx),
            ProjectMenuItem::Close => self.close_project(&menu.project, window, cx),
        }
        cx.notify();
    }

    /// The open project menu, if it belongs to `project`.
    pub(crate) fn render_project_menu(
        &self,
        project: &ProjectId,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectUp};

        let menu = self
            .session_management
            .project_menu
            .as_ref()
            .filter(|menu| &menu.project == project)?;
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let enabled = self.session_actions_available();
        let items = ProjectMenuItem::ALL
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let item = *item;
                let row = crate::session_menu::menu_row(
                    crate::session_menu::MenuRow {
                        id: format!("project-menu-item:{}", item.id()),
                        label: strings.get(item.label_key()),
                        hint: None,
                        icon: Some(item.icon()),
                        enabled,
                        highlighted: menu.highlighted == Some(index),
                        destructive: item == ProjectMenuItem::Close,
                        checked: None,
                        opens_submenu: false,
                    },
                    palette,
                    direction,
                    cx,
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(menu) = this.session_management.project_menu.as_mut()
                        && menu.highlighted != Some(index)
                    {
                        menu.highlighted = Some(index);
                        cx.notify();
                    }
                }));
                if enabled {
                    crate::activation::on_activate(row, cx, move |this, window, cx| {
                        this.run_project_menu_item(item, window, cx)
                    })
                    .test_support()
                    .into_any_element()
                } else {
                    row.test_support().into_any_element()
                }
            })
            .collect::<Vec<_>>();
        let step = |this: &mut Workspace, step: isize, cx: &mut Context<Workspace>| {
            if let Some(menu) = this.session_management.project_menu.as_mut() {
                let count = ProjectMenuItem::ALL.len();
                menu.highlighted = openchamber_native_core::session_actions::step_highlight_by(
                    count,
                    |_| true,
                    menu.highlighted,
                    step,
                );
                cx.notify();
            }
        };
        let popup = div()
            .id("project-menu")
            .role(Role::Menu)
            .accessibility_id("project-menu")
            .aria_label(strings.get("sessions.sidebar.project.actions.projectMenu"))
            .track_focus(&menu.focus)
            .key_context(crate::session_menu::MENU_CONTEXT)
            .on_action(cx.listener(move |this, _: &SelectDown, _, cx| step(this, 1, cx)))
            .on_action(cx.listener(move |this, _: &SelectUp, _, cx| step(this, -1, cx)))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                let item = this
                    .session_management
                    .project_menu
                    .as_ref()
                    .and_then(|menu| menu.highlighted)
                    .and_then(|index| ProjectMenuItem::ALL.get(index).copied());
                if let Some(item) = item.filter(|_| this.session_actions_available()) {
                    this.run_project_menu_item(item, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                this.session_management.project_menu = None;
                cx.notify();
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.session_management.project_menu = None;
                cx.notify();
            }))
            .occlude()
            .min_w(px(180.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(crate::elevation::floating(palette))
            .flex()
            .flex_col()
            .children(items)
            .test_support();
        let anchor = match direction {
            locale::Direction::LeftToRight => gpui_kit::Anchor::TopLeft,
            locale::Direction::RightToLeft => gpui_kit::Anchor::TopRight,
        };
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored()
                    .anchor(anchor)
                    .position(menu.position)
                    .position_mode(gpui_kit::AnchoredPositionMode::Window)
                    .snap_to_window_with_margin(gpui_kit::Edges::all(px(8.0)))
                    .child(popup),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// Reads the registry, changes it and writes it back. The answer's
    /// settings and topology are read again so every view follows.
    fn change_projects(
        &mut self,
        change: impl FnOnce(ProjectEntries) -> Option<ProjectEntries> + Send + 'static,
        // The web reports a failed add; other registry writes fail quietly.
        failure: Option<&'static str>,
        then: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.runtime.clone();
        self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    let registry = client.project_entries().await?;
                    let Some(next) = change(registry) else {
                        return Ok(false);
                    };
                    client.save_project_entries(&next).await.map(|_| true)
                })
            },
            move |this, result, window, cx| {
                if this.runtime != runtime {
                    return;
                }
                match result {
                    Some(Ok(_)) => {
                        this.load_topology(window, cx);
                        then(this, window, cx);
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the project registry could not be saved");
                        if let Some(failure) = failure {
                            let strings = locale::strings(cx).clone();
                            let detail =
                                crate::session_management::describe_error(&strings, &error);
                            this.push_error(failure, Some(detail), window, cx);
                        }
                    }
                    None => {}
                }
            },
            window,
            cx,
        );
    }

    /// Registers `directory` as a project and makes it active; a session
    /// waiting on the dialog moves there next.
    pub(crate) fn add_project(
        &mut self,
        directory: &str,
        move_after: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = path::normalize(directory) else {
            return;
        };
        let now = now_ms();
        let target = directory.clone();
        self.change_projects(
            move |registry| {
                with_project(registry, &target, now, random_index).map(|(next, _)| next)
            },
            Some("directoryExplorerDialog.toast.failedToAddProject"),
            move |this, window, cx| {
                if let Some(session) = move_after {
                    this.move_session_to_project(&session, &directory, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Close project: the project leaves the sidebar with its folders;
    /// nothing on disk changes.
    pub(crate) fn close_project(
        &mut self,
        project: &ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self
            .topology
            .projects
            .iter()
            .find(|candidate| &candidate.id == project)
            .map(|project| project.path.clone())
        else {
            return;
        };
        let id = project.as_str().to_owned();
        let worktrees: Vec<String> = self
            .topology
            .worktrees
            .iter()
            .filter(|listing| listing.project_path == root)
            .flat_map(|listing| {
                listing
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.clone())
            })
            .collect();
        self.change_projects(
            move |registry| Some(without_project(registry, &id)),
            None,
            move |this, _, cx| {
                let mut scopes = vec![root.clone(), format!("__archived__:{root}")];
                scopes.extend(worktrees);
                this.commit_folder_change(FolderChange::ForgetScopes { scopes }, cx);
            },
            window,
            cx,
        );
    }

    /// Add project from the sidebar toolbar, or Choose folder from the Move
    /// session dialog with the session to move afterwards.
    pub(crate) fn open_add_project(
        &mut self,
        move_after: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session_actions_available() {
            return;
        }
        let start = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.home_directory.clone())
            .unwrap_or_else(|| "/".to_owned());
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| {
            input.set_value(start.clone(), window, cx);
            input.focus(window, cx);
        });
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        let value = input.read(cx).value().to_string();
                        this.confirm_add_project(&value, window, cx);
                    }
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        this.list_add_project_folder(&value, cx);
                    }
                    _ => {}
                },
            );
        self.session_management.add_project = Some(AddProject {
            input,
            listed: None,
            entries: Ok(Vec::new()),
            move_after,
            adding: false,
            _subscription: subscription,
            _read: None,
        });
        self.list_add_project_folder(&start, cx);
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let state = this.read(cx);
            let Some(add) = state.session_management.add_project.as_ref() else {
                return dialog;
            };
            let input = add.input.clone();
            let hover = palette.hover;
            let listed = add.listed.clone();
            let rows: Vec<gpui_kit::AnyElement> = match &add.entries {
                Err(()) => vec![
                    div()
                        .px(px(8.0))
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(strings.get("directoryExplorerDialog.browse.loadFailed"))
                        .into_any_element(),
                ],
                Ok(entries) => {
                    let parent = listed.as_deref().and_then(path::parent);
                    parent
                        .map(|parent| (SharedString::from(".."), parent))
                        .into_iter()
                        .chain(
                            entries
                                .iter()
                                .filter(|entry| entry.kind == EntryKind::Directory)
                                .map(|entry| {
                                    (SharedString::from(entry.name.clone()), entry.path.clone())
                                }),
                        )
                        .map(|(name, target)| {
                            let workspace = workspace.clone();
                            let id = format!("add-project-folder:{target}");
                            div()
                                .id(SharedString::from(id.clone()))
                                .role(Role::Button)
                                .accessibility_id(id)
                                .aria_label(name.clone())
                                .px(px(8.0))
                                .py(px(4.0))
                                .rounded(px(6.0))
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(8.0))
                                .cursor_pointer()
                                .hover(move |style| style.bg(hover))
                                .child(glyph("folder", 16.0, palette.muted_foreground))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                                        .text_color(palette.foreground)
                                        .child(name),
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = workspace.update(cx, |this, cx| {
                                        if let Some(add) = &this.session_management.add_project {
                                            let input = add.input.clone();
                                            input.update(cx, |input, cx| {
                                                input.set_value(target.clone(), window, cx)
                                            });
                                        }
                                        this.list_add_project_folder(&target, cx);
                                    });
                                })
                                .test_support()
                                .into_any_element()
                        })
                        .collect()
                }
            };
            let adding = add.adding;
            let on_add = workspace.clone();
            let on_cancel = workspace.clone();
            let on_close = workspace.clone();
            dialog
                .w(px(480.0))
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.session_management.add_project = None;
                        cx.notify();
                    });
                })
                .title(strings.get("directoryExplorerDialog.actions.addProject"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(Input::new(&input))
                        .child(
                            div()
                                .id("add-project-folders")
                                .h(px(280.0))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .children(rows),
                        ),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            dialog_button(
                                "add-project-cancel",
                                strings.get("sessions.sidebar.dialogs.cancel"),
                                None,
                                palette.foreground,
                                &palette,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_cancel.update(cx, |this, cx| {
                                    this.session_management.add_project = None;
                                    window.close_dialog(cx);
                                    cx.notify();
                                });
                            }),
                        )
                        .child(
                            dialog_button(
                                "add-project-confirm",
                                strings.get("directoryExplorerDialog.actions.addProject"),
                                Some(palette.primary),
                                palette.primary_foreground,
                                &palette,
                            )
                            .when(adding, |this| this.opacity(0.5))
                            .on_click(move |_, window, cx| {
                                let _ = on_add.update(cx, |this, cx| {
                                    let Some(value) = this
                                        .session_management
                                        .add_project
                                        .as_ref()
                                        .map(|add| add.input.read(cx).value().to_string())
                                    else {
                                        return;
                                    };
                                    this.confirm_add_project(&value, window, cx);
                                });
                            }),
                        ),
                )
        });
        cx.notify();
    }

    /// Lists the subfolders of the typed folder. A path that cannot be read
    /// says so; the typed path can still be added.
    fn list_add_project_folder(&mut self, folder: &str, cx: &mut Context<Self>) {
        let Some(folder) = path::normalize(folder) else {
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let target = folder.clone();
        let read = connection
            .network
            .run(async move { client.list_directory(&target, &target, false).await });
        let runtime = self.runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                let Some(add) = this.session_management.add_project.as_mut() else {
                    return;
                };
                add.listed = Some(folder);
                add.entries = match result {
                    Some(Ok(mut entries)) => {
                        entries.sort_by_key(|entry| entry.name.to_lowercase());
                        Ok(entries)
                    }
                    _ => Err(()),
                };
                cx.notify();
            });
        });
        if let Some(add) = self.session_management.add_project.as_mut() {
            add._read = Some(task);
        }
    }

    fn confirm_add_project(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(add) = self.session_management.add_project.as_mut() else {
            return;
        };
        if add.adding || path::normalize(value).is_none() {
            return;
        }
        let move_after = add.move_after.clone();
        self.session_management.add_project = None;
        window.close_dialog(cx);
        self.add_project(value, move_after, window, cx);
    }

    /// Edit from a project's menu: the label the sidebar shows.
    pub(crate) fn open_edit_project(
        &mut self,
        project: &ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self
            .topology
            .projects
            .iter()
            .find(|candidate| &candidate.id == project)
            .map(|project| project.path.clone())
        else {
            return;
        };
        let label = self.project_display_label(&root);
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| {
            input.set_value(label, window, cx);
            input.select_all(window, cx);
            input.focus(window, cx);
        });
        self.session_management.edit_project = Some(EditProject {
            project: project.as_str().to_owned(),
            input: input.clone(),
        });
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let on_save = workspace.clone();
            let on_close = workspace.clone();
            dialog
                .w(px(384.0))
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.session_management.edit_project = None;
                        cx.notify();
                    });
                })
                .title(strings.get("sessions.sidebar.project.actions.edit"))
                .child(Input::new(&input))
                .footer(
                    div().w_full().flex_row_from(direction).justify_end().child(
                        dialog_button(
                            "edit-project-save",
                            strings.get("sessions.sidebar.session.rename.save"),
                            Some(palette.primary),
                            palette.primary_foreground,
                            &palette,
                        )
                        .on_click(move |_, window, cx| {
                            let _ =
                                on_save.update(cx, |this, cx| this.save_edit_project(window, cx));
                        }),
                    ),
                )
        });
        cx.notify();
    }

    fn save_edit_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.session_management.edit_project.take() else {
            return;
        };
        let label = edit.input.read(cx).value().to_string();
        window.close_dialog(cx);
        let id = edit.project;
        self.change_projects(
            move |registry| Some(with_label(registry, &id, &label)),
            None,
            |_, _, _| {},
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(paths: &[(&str, &str)]) -> ProjectEntries {
        ProjectEntries {
            entries: paths
                .iter()
                .map(|(path, color)| {
                    let Value::Object(entry) = json!({
                        "id": ProjectId::from_path(path).expect("a path").as_str(),
                        "path": path,
                        "color": color,
                        "icon": "star",
                    }) else {
                        unreachable!("an object literal")
                    };
                    entry
                })
                .collect(),
            active_project_id: None,
        }
    }

    #[test]
    fn labels_are_folder_names() {
        assert_eq!(derive_project_label("/home/u/app/"), "app");
        assert_eq!(derive_project_label("/"), "Root");
    }

    #[test]
    fn a_new_project_takes_a_least_used_colour_and_becomes_active() {
        let stored = registry(&[("/a", "#e5484d")]);
        let (next, id) = with_project(stored, "/b", 5, |_| 0).expect("a valid path");
        assert_eq!(next.entries.len(), 2);
        assert_eq!(next.entries[1]["color"], json!("#f76b15"));
        assert_eq!(next.entries[1]["labelSource"], json!("auto"));
        assert_eq!(next.active_project_id.as_deref(), Some(id.as_str()));
        let (again, same) = with_project(next, "/b", 6, |_| 0).expect("a valid path");
        assert_eq!(
            again.entries.len(),
            2,
            "an existing project is only activated"
        );
        assert_eq!(same, id);
    }

    #[test]
    fn closing_and_renaming_keep_the_other_fields() {
        let stored = registry(&[("/a", "#e5484d"), ("/b", "#f76b15")]);
        let a = ProjectId::from_path("/a").expect("a path");
        let renamed = with_label(stored.clone(), a.as_str(), " App ");
        assert_eq!(renamed.entries[0]["label"], json!("App"));
        assert_eq!(renamed.entries[0]["icon"], json!("star"));
        assert_eq!(with_label(stored.clone(), a.as_str(), "  "), stored);
        let mut active = stored;
        active.active_project_id = Some(a.as_str().to_owned());
        let closed = without_project(active, a.as_str());
        assert_eq!(closed.entries.len(), 1);
        assert_eq!(
            closed.active_project_id.as_deref(),
            Some(ProjectId::from_path("/b").expect("a path").as_str())
        );
    }
}
