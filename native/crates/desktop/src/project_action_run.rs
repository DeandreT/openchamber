//! The header's project action button (`HeaderProjectControls.tsx`) and the
//! runner behind it (`useProjectActionsRunner.ts`): one button runs or stops
//! the shown action, a secondary click lists every action, and a shared
//! action from the repository runs only after the shared-setup trust prompt
//! (`lib/sharedTrustConfirmation.ts`).
//!
//! Runs live in the terminal panel, one tab per action and directory, so
//! they keep running while the panel is hidden; the panel lists the
//! directory's terminals when the button first shows and whenever the server
//! says its terminals changed, so a run another client started shows as
//! running here.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use gpui_kit::component::WindowExt;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    Anchor, AnchoredPositionMode, AnyElement, Context, Edges, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Role, SharedString, StatefulInteractiveElement,
    Styled, TestSupportExt, Window, anchored, deferred, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::project_actions::{
    self, ActionSource, PackageManager, ProjectAction, SharedTrust,
};
use openchamber_native_core::terminal_tabs::ActionRun;
use openchamber_native_transport::terminal::ActionCommand;

use crate::activation::on_activate;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::elevation;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::session_export::dialog_button;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The header's 28 px icon buttons with 16 px glyphs.
const BUTTON: f32 = 28.0;

/// The button's state outside the readouts.
#[derive(Default)]
pub(crate) struct ProjectActionButton {
    /// Where the secondary-click menu opened.
    menu: Option<Point<Pixels>>,
    /// The trust prompt's question while it is open.
    trust: Option<PendingTrust>,
}

struct PendingTrust {
    project: String,
    trust: SharedTrust,
    action: ProjectAction,
}

/// What the button is about: the session's directory, the project whose
/// actions it offers and their list with auto-discovery last.
struct ActionScope {
    directory: String,
    project: String,
    actions: Vec<ProjectAction>,
    untrusted: Option<SharedTrust>,
}

/// A project action as the command palette offers it: what the row does
/// (Stop while the action runs, Run otherwise) and the project and session
/// directory it was listed for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaletteProjectAction {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) glyph: &'static str,
    pub(crate) running: bool,
    pub(crate) project: String,
    pub(crate) directory: String,
}

/// How an action stands when a palette row is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunState {
    Idle,
    Running,
    Stopping,
}

/// What a chosen palette row does to its action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteStep {
    Run,
    Stop,
}

/// The step a palette row may still take: the one it showed, in the project
/// and directory it showed it for. A Stop row whose action has ended starts
/// nothing, a Run row whose action started meanwhile stops nothing, and a
/// row from another project or directory does nothing.
fn palette_step(
    row: &PaletteProjectAction,
    project: &str,
    directory: &str,
    state: RunState,
) -> Option<PaletteStep> {
    if row.project != project || row.directory != directory {
        return None;
    }
    match (row.running, state) {
        (true, RunState::Running) => Some(PaletteStep::Stop),
        (false, RunState::Idle) => Some(PaletteStep::Run),
        _ => None,
    }
}

impl ActionScope {
    /// `runIn: 'parent'` runs in the project's checkout; everything else in
    /// the session's directory.
    fn directory_of(&self, action: &ProjectAction) -> &str {
        if action.run_in_parent && !action.is_auto_discover() {
            &self.project
        } else {
            &self.directory
        }
    }
}

impl Workspace {
    fn action_scope(&self, strings: &Strings) -> Option<ActionScope> {
        let subject = self.project_subject()?;
        let project = subject.action_project?;
        let setup = self.project_setup(&project);
        let mut actions = setup.map(|setup| setup.actions.clone()).unwrap_or_default();
        actions.push(ProjectAction::auto_discover(
            strings
                .get("projectActions.actions.autoDiscover")
                .to_string(),
        ));
        Some(ActionScope {
            directory: subject.directory,
            project,
            actions,
            untrusted: setup.and_then(|setup| setup.untrusted.clone()),
        })
    }

    fn run_of(
        &self,
        scope: &ActionScope,
        action: &ProjectAction,
        cx: &gpui_kit::App,
    ) -> Option<ActionRun> {
        self.terminal_panel
            .as_ref()?
            .read(cx)
            .action_run(scope.directory_of(action), &action.id)
    }

    /// Keeps the shown project's runs current and its action tabs named.
    pub(crate) fn ensure_project_action_runs(&mut self, cx: &mut Context<Self>) {
        let strings = locale::strings(cx).clone();
        let Some(scope) = self.action_scope(&strings) else {
            return;
        };
        if self
            .connection
            .as_ref()
            .and_then(|connection| connection.client.as_ref())
            .is_none()
        {
            return;
        }
        // The panel starts with the first run or the first time the terminal
        // opens; until then a run another client started is joined by the
        // server when Run is pressed, never started twice.
        let Some(panel) = self.terminal_panel.clone() else {
            return;
        };
        panel.update(cx, |panel, cx| {
            for directory in [scope.directory.as_str(), scope.project.as_str()] {
                panel.observe(directory, cx);
                panel.name_actions(
                    directory,
                    scope
                        .actions
                        .iter()
                        .map(|action| (action.id.as_str(), action.name.as_str(), action.glyph())),
                );
            }
        });
    }

    /// The action the button shows: a running one, else the stored pick,
    /// else the first.
    fn shown_action<'a>(
        &self,
        scope: &'a ActionScope,
        cx: &gpui_kit::App,
    ) -> Option<&'a ProjectAction> {
        let picked = cx
            .try_global::<crate::ui_memory::UiMemory>()
            .and_then(|memory| memory.action_pick(self.runtime.as_str(), &scope.project));
        project_actions::shown(&scope.actions, picked, |action| {
            self.run_of(scope, action, cx).is_some()
        })
    }

    /// The shown project's actions as the command palette lists them. An
    /// action that is stopping is left out: there is nothing to do with it.
    pub(crate) fn palette_project_actions(&self, cx: &gpui_kit::App) -> Vec<PaletteProjectAction> {
        let strings = locale::strings(cx).clone();
        let Some(scope) = self.action_scope(&strings) else {
            return Vec::new();
        };
        scope
            .actions
            .iter()
            .filter_map(|action| {
                let run = self.run_of(&scope, action, cx);
                if run.as_ref().is_some_and(|run| run.stopping) {
                    return None;
                }
                Some(PaletteProjectAction {
                    id: action.id.clone(),
                    name: action.name.clone(),
                    glyph: if run.is_some() {
                        "stop"
                    } else {
                        action.glyph()
                    },
                    running: run.is_some(),
                    project: scope.project.clone(),
                    directory: scope.directory.clone(),
                })
            })
            .collect()
    }

    /// Runs or stops the action of a chosen palette row, when the row still
    /// says what would happen (`palette_step`).
    pub(crate) fn run_palette_project_action(
        &mut self,
        row: &PaletteProjectAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        let Some(scope) = self.action_scope(&strings) else {
            return;
        };
        let Some(action) = scope
            .actions
            .iter()
            .find(|action| action.id == row.id)
            .cloned()
        else {
            return;
        };
        let state = match self.run_of(&scope, &action, cx) {
            Some(run) if run.stopping => RunState::Stopping,
            Some(_) => RunState::Running,
            None => RunState::Idle,
        };
        match palette_step(row, &scope.project, &scope.directory, state) {
            Some(PaletteStep::Stop) => {
                let directory = scope.directory_of(&action).to_owned();
                if let Some(panel) = &self.terminal_panel {
                    panel.update(cx, |panel, cx| {
                        panel.stop_action(&directory, &action.id, cx)
                    });
                }
            }
            Some(PaletteStep::Run) => self.run_with_trust(&scope, action, window, cx),
            None => return,
        }
        cx.notify();
    }

    /// Stops a running action, runs an idle one, and ignores one already
    /// stopping (`toggle`).
    fn toggle_project_action(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx).clone();
        let Some(scope) = self.action_scope(&strings) else {
            return;
        };
        let Some(action) = scope.actions.iter().find(|action| action.id == id).cloned() else {
            return;
        };
        match self.run_of(&scope, &action, cx) {
            Some(run) if run.stopping => {}
            Some(_) => {
                let directory = scope.directory_of(&action).to_owned();
                if let Some(panel) = &self.terminal_panel {
                    panel.update(cx, |panel, cx| {
                        panel.stop_action(&directory, &action.id, cx)
                    });
                }
            }
            None => self.run_with_trust(&scope, action, window, cx),
        }
        cx.notify();
    }

    /// A shared action asks for trust the first time its commands would
    /// run; "Not this time" runs nothing.
    fn run_with_trust(
        &mut self,
        scope: &ActionScope,
        action: ProjectAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.source == ActionSource::Shared
            && let Some(trust) = scope.untrusted.clone()
        {
            self.ask_trust(scope.project.clone(), trust, action, window, cx);
            return;
        }
        if action.is_auto_discover() {
            self.discover_and_run(scope, window, cx);
        } else {
            self.launch_action(scope, &action, &action.command, window, cx);
        }
    }

    /// Auto-discovery: a dev-server action, a dev script with the project's
    /// package manager, or a static file server for an `index.html`
    /// (`detectDevServerCommand`).
    fn discover_and_run(
        &mut self,
        scope: &ActionScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let directory = scope.directory.clone();
        let actions: Vec<ProjectAction> = scope
            .actions
            .iter()
            .filter(|action| !action.is_auto_discover())
            .cloned()
            .collect();
        let read = connection.network.run(async move {
            let text = |name: &str| {
                let client = client.clone();
                let path = format!("{}/{name}", directory.trim_end_matches('/'));
                let directory = directory.clone();
                async move {
                    client
                        .read_file(&directory, &path, false)
                        .await
                        .ok()
                        .filter(|text| !text.trim().is_empty())
                }
            };
            let package = text("package.json").await;
            let parsed: Option<serde_json::Value> = package
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok());
            let scripts: Vec<String> = parsed
                .as_ref()
                .and_then(|value| value.get("scripts"))
                .and_then(serde_json::Value::as_object)
                .map(|scripts| scripts.keys().cloned().collect())
                .unwrap_or_default();
            let mut manager = parsed
                .as_ref()
                .and_then(|value| value.get("packageManager"))
                .and_then(serde_json::Value::as_str)
                .and_then(PackageManager::from_package_field);
            if manager.is_none() && !scripts.is_empty() {
                for (file, candidate) in PackageManager::LOCK_FILES {
                    if text(file).await.is_some() {
                        manager = Some(candidate);
                        break;
                    }
                }
            }
            let found = project_actions::dev_server(
                &actions,
                &scripts,
                manager.unwrap_or(PackageManager::Npm),
            );
            if found.is_some() {
                return found;
            }
            text("index.html").await?;
            let port = client.free_port().await.ok().flatten().unwrap_or(8000);
            Some(project_actions::DevServer {
                command: format!("python3 -m http.server {port}"),
                action: None,
            })
        });
        let runtime = self.runtime.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(found) = read.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.runtime != runtime {
                    return;
                }
                let strings = locale::strings(cx).clone();
                let Some(scope) = this.action_scope(&strings) else {
                    return;
                };
                let Some(found) = found else {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error(
                            strings.get("contextPanel.preview.noDevServer"),
                        ),
                        cx,
                    );
                    return;
                };
                let auto = ProjectAction::auto_discover(
                    strings
                        .get("projectActions.actions.autoDiscover")
                        .to_string(),
                );
                // A reused shared action keeps its trust requirement.
                if let Some(reused) = &found.action
                    && reused.source == ActionSource::Shared
                    && let Some(trust) = scope.untrusted.clone()
                {
                    let mut pending = auto;
                    pending.command = found.command;
                    this.ask_trust(scope.project.clone(), trust, pending, window, cx);
                    return;
                }
                this.launch_action(&scope, &auto, &found.command, window, cx);
            });
        })
        .detach();
    }

    /// Starts `command` as `action`'s run and reveals its terminal.
    fn launch_action(
        &mut self,
        scope: &ActionScope,
        action: &ProjectAction,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = project_actions::normalize_command(command);
        if command.is_empty() {
            window.push_notification(
                gpui_kit::component::notification::Notification::error(
                    locale::strings(cx).get("projectActions.error.failedToRunAction"),
                ),
                cx,
            );
            return;
        }
        self.ensure_terminal_panel(cx);
        let Some(panel) = self.terminal_panel.clone() else {
            return;
        };
        let directory = scope.directory_of(action).to_owned();
        let launch = ActionCommand {
            command,
            action_id: action.id.clone(),
            execution_id: execution_id(),
        };
        panel.update(cx, |panel, cx| {
            panel.run_action(&directory, launch, &action.name, action.glyph(), cx)
        });
        self.reveal_terminal(Some(scope.directory.clone()), &directory, window, cx);
    }

    fn pick_action(&mut self, project: &str, action: &str, cx: &mut Context<Self>) {
        crate::ui_memory::pick_action(self.runtime.as_str(), project, action, cx);
    }

    fn ask_trust(
        &mut self,
        project: String,
        trust: SharedTrust,
        action: ProjectAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_action_button.trust = Some(PendingTrust {
            project,
            trust,
            action,
        });
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let Some(pending) = this.read(cx).project_action_button.trust.as_ref() else {
                return dialog;
            };
            let on_close = workspace.clone();
            let on_skip = workspace.clone();
            let on_trust = workspace.clone();
            let list = |title: SharedString, commands: Vec<String>| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(gpui_kit::rems(0.75))
                            .text_color(palette.muted_foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .p(px(8.0))
                            .rounded(px(8.0))
                            .bg(palette.muted)
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .children(commands.into_iter().map(|command| {
                                div()
                                    .font_family(
                                        gpui_kit::component::Theme::global(cx)
                                            .mono_font_family
                                            .clone(),
                                    )
                                    .text_size(gpui_kit::rems(0.75))
                                    .text_color(palette.foreground)
                                    .child(command)
                            })),
                    )
            };
            let setup = pending.trust.setup_commands.clone();
            let actions: Vec<String> = pending
                .trust
                .actions
                .iter()
                .map(|(name, command)| format!("{name}: {command}"))
                .collect();
            dialog
                .w(px(448.0))
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        // Closing skips the shared commands: nothing runs.
                        this.project_action_button.trust = None;
                        cx.notify();
                    });
                })
                .title(strings.get("projects.sharedTrust.title"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(
                            div()
                                .text_size(gpui_kit::rems(0.843_75))
                                .text_color(palette.muted_foreground)
                                .child(strings.format(
                                    "projects.sharedTrust.description",
                                    &[("path", &pending.trust.path)],
                                )),
                        )
                        .when(!setup.is_empty(), |this| {
                            this.child(list(
                                strings.get("projects.sharedTrust.setupCommands"),
                                setup,
                            ))
                        })
                        .when(!actions.is_empty(), |this| {
                            this.child(list(strings.get("projects.sharedTrust.actions"), actions))
                        }),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            dialog_button(
                                "shared-trust-skip",
                                strings.get("projects.sharedTrust.skip"),
                                None,
                                palette.foreground,
                                &palette,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_skip.update(cx, |this, cx| {
                                    this.project_action_button.trust = None;
                                    window.close_dialog(cx);
                                    cx.notify();
                                });
                            }),
                        )
                        .child(
                            dialog_button(
                                "shared-trust-trust",
                                strings.get("projects.sharedTrust.trust"),
                                Some(palette.primary),
                                palette.primary_foreground,
                                &palette,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_trust.update(cx, |this, cx| {
                                    window.close_dialog(cx);
                                    this.trust_and_run(window, cx);
                                });
                            }),
                        ),
                )
        });
        cx.notify();
    }

    /// Records the trust answer and runs the action. A failed record still
    /// runs it this once: the user said yes to exactly these commands.
    fn trust_and_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.project_action_button.trust.take() else {
            return;
        };
        self.trusted_shared_setup(&pending.project, &pending.trust.hash, cx);
        let strings = locale::strings(cx).clone();
        let Some(scope) = self.action_scope(&strings) else {
            return;
        };
        // Auto-discovery asks once it found a shared action, so its command
        // is known by now.
        self.launch_action(&scope, &pending.action, &pending.action.command, window, cx);
    }

    /// The header button with the shown action's glyph while idle and its
    /// stop while running; a secondary click lists every action.
    pub(crate) fn render_project_action(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let strings = locale::strings(cx);
        let scope = self.action_scope(strings)?;
        let shown = self.shown_action(&scope, cx)?.clone();
        let run = self.run_of(&scope, &shown, cx);
        let label = strings.format(
            if run.is_some() {
                "projectActions.actions.stopNamedAria"
            } else {
                "projectActions.actions.runNamedAria"
            },
            &[("name", &shown.name)],
        );
        let tooltip = label.clone();
        let all = strings.get("projectActions.header.allActions");
        let id = shown.id.clone();
        let stopping = run.as_ref().is_some_and(|run| run.stopping);
        let icon = run_glyph(run.as_ref(), true).unwrap_or(shown.glyph());
        let tone = if run.is_some() {
            palette.status_warning
        } else {
            palette.muted_foreground
        };
        let foreground = palette.foreground;
        let button = div()
            .id("project-action")
            .role(Role::Button)
            .accessibility_id("project-action")
            .aria_label(label)
            .tooltip(move |window, cx| Tooltip::new(format!("{tooltip}\n{all}")).build(window, cx))
            .size(px(BUTTON))
            .flex_shrink_0()
            .rounded(px(CONTROL_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .when(!stopping, |this| this.cursor_pointer())
            .when(stopping, |this| this.opacity(0.5))
            .group("project-action")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.project_action_button.menu = Some(event.position);
                    cx.notify();
                }),
            )
            .child(glyph(icon, 16.0, tone).when(run.is_none(), |this| {
                this.group_hover("project-action", move |style| style.text_color(foreground))
            }));
        let button = on_activate(button, cx, move |this: &mut Self, window, cx| {
            this.toggle_project_action(&id, window, cx)
        })
        .test_support();
        Some(
            div()
                .flex_row_from(direction)
                .items_center()
                .child(button)
                .children(self.render_project_action_menu(&scope, palette, cx))
                .into_any_element(),
        )
    }

    fn render_project_action_menu(
        &self,
        scope: &ActionScope,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let position = self.project_action_button.menu?;
        let strings = locale::strings(cx);
        let item = |id: SharedString, label: SharedString| {
            div()
                .id(id.clone())
                .role(Role::MenuItem)
                .accessibility_id(id)
                .aria_label(label)
                .h(px(28.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .hover(|style| style.bg(palette.hover))
        };
        let add = on_activate(
            item(
                "project-action-add".into(),
                strings.get("projectActions.actions.addNewAction"),
            )
            .child(glyph("add", 16.0, palette.foreground))
            .child(
                div()
                    .text_size(gpui_kit::rems(0.8125))
                    .text_color(palette.foreground)
                    .child(strings.get("projectActions.actions.addNewAction")),
            ),
            cx,
            |this: &mut Self, _, cx| {
                // The project settings page is not ported yet.
                tracing::info!("Add new action needs the project settings page");
                this.project_action_button.menu = None;
                cx.notify();
            },
        )
        .test_support();
        let rows = scope.actions.iter().map(|action| {
            let run = self.run_of(scope, action, cx);
            let id = action.id.clone();
            let project = scope.project.clone();
            let row = item(
                SharedString::from(format!("project-action-item:{}", action.id)),
                action.name.clone().into(),
            )
            .child(glyph(action.glyph(), 16.0, palette.foreground))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(gpui_kit::rems(0.8125))
                    .text_color(palette.foreground)
                    .child(action.name.clone()),
            )
            .when(action.source == ActionSource::Shared, |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .px(px(4.0))
                        .rounded(px(4.0))
                        .bg(palette.muted)
                        .text_size(gpui_kit::rems(0.6875))
                        .text_color(palette.muted_foreground)
                        .child(strings.get("projectActions.menu.sharedBadge")),
                )
            })
            .children(
                run_glyph(run.as_ref(), false)
                    .map(|icon| glyph(icon, 16.0, palette.status_warning)),
            );
            on_activate(row, cx, move |this: &mut Self, window, cx| {
                this.project_action_button.menu = None;
                this.pick_action(&project, &id, cx);
                this.toggle_project_action(&id, window, cx);
            })
            .test_support()
            .into_any_element()
        });
        let popup = div()
            .id("project-action-menu")
            .role(Role::Menu)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.project_action_button.menu = None;
                cx.notify();
            }))
            .w(px(240.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .flex()
            .flex_col()
            .child(add)
            .child(div().my(px(4.0)).h(px(1.0)).bg(palette.border))
            .children(rows);
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .position(position)
                    .position_mode(AnchoredPositionMode::Window)
                    .snap_to_window_with_margin(Edges::all(px(8.0)))
                    .child(popup),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

/// `ProjectActionRunIndicator`: what acting on the action does next.
fn run_glyph(run: Option<&ActionRun>, large: bool) -> Option<&'static str> {
    match run {
        Some(run) if run.stopping => Some("loader-4"),
        Some(_) if large => Some("stop-large"),
        Some(_) => Some("stop"),
        None => None,
    }
}

/// A fresh run's ID (`createExecutionId`): `exec-` and 32 hex digits from
/// the standard library's per-process random hasher keys. It names the run,
/// never a credential.
fn execution_id() -> String {
    let mut id = String::from("exec-");
    for half in 0u8..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(half);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
        );
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette_row(running: bool) -> PaletteProjectAction {
        PaletteProjectAction {
            id: "dev".to_owned(),
            name: "Dev server".to_owned(),
            glyph: "play",
            running,
            project: "/work/app".to_owned(),
            directory: "/work/app-worktree".to_owned(),
        }
    }

    #[test]
    fn a_palette_row_does_only_what_it_showed() {
        let step = |running, state| {
            palette_step(
                &palette_row(running),
                "/work/app",
                "/work/app-worktree",
                state,
            )
        };
        assert_eq!(step(false, RunState::Idle), Some(PaletteStep::Run));
        assert_eq!(step(true, RunState::Running), Some(PaletteStep::Stop));
        assert_eq!(
            step(true, RunState::Idle),
            None,
            "a Stop row whose action has ended starts nothing"
        );
        assert_eq!(
            step(false, RunState::Running),
            None,
            "a Run row whose action started meanwhile stops nothing"
        );
        assert_eq!(step(true, RunState::Stopping), None);
        assert_eq!(step(false, RunState::Stopping), None);
    }

    #[test]
    fn a_palette_row_does_nothing_in_another_project_or_directory() {
        for running in [false, true] {
            let state = if running {
                RunState::Running
            } else {
                RunState::Idle
            };
            let row = palette_row(running);
            assert_eq!(
                palette_step(&row, "/work/other", "/work/app-worktree", state),
                None,
                "the shown project changed"
            );
            assert_eq!(
                palette_step(&row, "/work/app", "/work/app", state),
                None,
                "the session directory changed"
            );
            assert!(palette_step(&row, "/work/app", "/work/app-worktree", state).is_some());
        }
    }

    #[test]
    fn execution_ids_are_distinct_and_within_the_server_limit() {
        let first = execution_id();
        let second = execution_id();
        assert_ne!(first, second);
        assert!(first.starts_with("exec-") && first.len() <= 128);
    }
}
