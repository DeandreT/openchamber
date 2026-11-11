//! The Git header's Update branch dialog (`GitHeader.tsx`'s repository
//! views menu, the `bare` form of `BranchIntegrationSection.tsx` and
//! `handleBranchIntegration` in `GitView.tsx`).
//!
//! The form picks Merge or Rebase and a local or remote-tracking branch,
//! starting at the base branch. A remote-tracking branch is fetched first as
//! a planned network operation of that one ref, then integrated at the
//! commit the fetch left in its tracking ref. The run's steps show as a log;
//! the dialog stays open until Done or Close clears it, and a conflict opens
//! the conflict dialog over it. The log's messages are English in the
//! reference too.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, EntityInputHandler, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, Task,
    TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::branch_integration::{
    self, IntegrationTarget, RemoteTarget, default_target, filter_targets, remote_targets,
};
use openchamber_native_core::git_changes::{Integration, OperationState, ref_fetch_request};
use openchamber_native_transport::git::IntegrationOutcome;

use super::GitView;
use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::elevation;
use crate::files_view::{PANEL_ROW, text_button};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogStatus {
    Running,
    Done,
    Error,
}

struct LogEntry {
    message: SharedString,
    status: LogStatus,
}

/// The repository views menu and the Update branch dialog.
#[derive(Default)]
pub(super) struct UpdateBranch {
    menu_open: bool,
    open: bool,
    operation: Option<Integration>,
    selected: Option<IntegrationTarget>,
    picker: Option<Entity<InputState>>,
    trigger_focus: Option<gpui_kit::FocusHandle>,
    highlighted: usize,
    picker_scroll: gpui_kit::ScrollHandle,
    remote_names: Vec<String>,
    /// The run's steps; kept after it ends until Done or Close.
    logs: Vec<LogEntry>,
    /// A run started and was not cleared yet (`branchOperation`).
    operating: bool,
    run: Option<Task<()>>,
    remotes_read: Option<Task<()>>,
    fetch_id: Option<String>,
    fetch_events: Option<Task<()>>,
    fetch_cancel: Option<Task<()>>,
}

/// Why a step of the run failed, with the text its log line shows.
struct StepFailure(String);

impl UpdateBranch {
    fn finished(&self) -> bool {
        !self.logs.is_empty() && self.logs.iter().all(|log| log.status != LogStatus::Running)
    }

    fn log(&mut self, message: impl Into<SharedString>) {
        self.logs.push(LogEntry {
            message: message.into(),
            status: LogStatus::Running,
        });
    }

    fn settle(&mut self, status: LogStatus, message: Option<String>) {
        if let Some(last) = self.logs.last_mut() {
            last.status = status;
            if let Some(message) = message {
                last.message = message.into();
            }
        }
    }
}

impl GitView {
    fn remote_targets(&self) -> Vec<RemoteTarget> {
        let labels = self
            .branches
            .as_ref()
            .map(|branches| branches.remote.clone())
            .unwrap_or_default();
        remote_targets(&self.update.remote_names, &labels)
    }

    fn default_update_target(&self) -> Option<IntegrationTarget> {
        let branches = self.branches.as_ref()?;
        let current = self.changes().and_then(|changes| changes.branch.as_deref());
        let tracked = self
            .changes()
            .and_then(|changes| changes.tracking.as_deref())
            .and_then(|tracking| {
                self.update
                    .remote_names
                    .iter()
                    .filter(|name| {
                        tracking
                            .strip_prefix(name.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                    })
                    .max_by_key(|name| name.len())
            });
        let primary = tracked
            .or_else(|| self.update.remote_names.first())
            .map(String::as_str);
        let default_branch = primary.and_then(|remote| branches.default_branches.get(remote));
        default_target(
            primary,
            default_branch.map(String::as_str),
            current,
            &branches.local,
            &self.remote_targets(),
        )
    }

    fn toggle_repository_views(&mut self, cx: &mut Context<Self>) {
        self.update.menu_open = !self.update.menu_open;
        cx.notify();
    }

    fn open_update_branch(&mut self, cx: &mut Context<Self>) {
        self.update
            .trigger_focus
            .get_or_insert_with(|| cx.focus_handle());
        self.update.menu_open = false;
        self.update.open = true;
        self.update.operation = Some(Integration::Merge);
        self.read_branches(cx);
        self.read_remote_names(cx);
        self.update.selected = self.default_update_target();
        cx.notify();
    }

    /// Closing waits while a run is shown, as the reference keeps its log up.
    fn close_update_branch(&mut self, cx: &mut Context<Self>) {
        if self.update.operating {
            return;
        }
        self.update.open = false;
        self.update.picker = None;
        self.update.selected = None;
        cx.notify();
    }

    fn read_remote_names(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let task = self
            .scope
            .run(async move { client.remote_names(&directory).await });
        self.update.remotes_read = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.update.remotes_read = None;
                match result {
                    Ok(names) => this.update.remote_names = names,
                    Err(error) => tracing::debug!(%error, "the remotes could not be read"),
                }
                if this.update.selected.is_none() && !this.update.operating {
                    this.update.selected = this.default_update_target();
                }
                cx.notify();
            });
        }));
    }

    /// The branches were read again; a dialog without a choice takes the
    /// base branch, as the bare form does.
    pub(super) fn branches_read_for_update(&mut self) {
        if self.update.open && self.update.selected.is_none() && !self.update.operating {
            self.update.selected = self.default_update_target();
        }
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update.picker.take().is_some() {
            cx.notify();
            return;
        }
        let placeholder = locale::strings(cx).get("gitView.branch.searchPlaceholder");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::Change = event {
                this.update.highlighted = 0;
                this.update
                    .picker_scroll
                    .set_offset(gpui_kit::point(px(0.0), px(0.0)));
                cx.notify();
            }
        });
        self._subscriptions.push(subscription);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.update.picker = Some(input);
        self.update.highlighted = 0;
        cx.notify();
    }

    fn choose_target(&mut self, target: IntegrationTarget, cx: &mut Context<Self>) {
        self.update.selected = Some(target);
        self.update.picker = None;
        cx.notify();
    }

    fn picker_targets(&self, cx: &Context<Self>) -> Vec<IntegrationTarget> {
        let Some(input) = &self.update.picker else {
            return Vec::new();
        };
        let local = self
            .branches
            .as_ref()
            .map(|branches| branches.local.clone())
            .unwrap_or_default();
        let remote = self.remote_targets();
        let current = self.changes().and_then(|changes| changes.branch.as_deref());
        let query = input.read(cx).value();
        let (local, remote) = filter_targets(&local, &remote, current, &query);
        local
            .into_iter()
            .cloned()
            .map(IntegrationTarget::Local)
            .chain(remote.into_iter().cloned().map(IntegrationTarget::Remote))
            .collect()
    }

    fn update_picker_key(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.update.picker.clone() else {
            return;
        };
        if input.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        }) {
            return;
        }
        let key = &event.keystroke;
        if key.modifiers.alt || key.modifiers.platform || key.modifiers.shift {
            return;
        }
        let targets = self.picker_targets(cx);
        match (key.key.as_str(), key.modifiers.control) {
            ("down", false) | ("n", true) if !targets.is_empty() => {
                self.update.highlighted = (self.update.highlighted + 1) % targets.len()
            }
            ("up", false) | ("p", true) if !targets.is_empty() => {
                self.update.highlighted =
                    (self.update.highlighted + targets.len() - 1) % targets.len()
            }
            ("enter", false) => {
                if let Some(target) = targets.get(self.update.highlighted) {
                    self.choose_target(target.clone(), cx);
                }
            }
            ("escape", false) => {
                self.update.picker = None;
                if let Some(focus) = &self.update.trigger_focus {
                    window.focus(focus, cx);
                }
            }
            _ => return,
        }
        // Heading elements occupy their own slots in the scroll container.
        let local_count = targets
            .iter()
            .filter(|target| matches!(target, IntegrationTarget::Local(_)))
            .count();
        let headings =
            usize::from(local_count > 0) + usize::from(self.update.highlighted >= local_count);
        self.update
            .picker_scroll
            .scroll_to_item(self.update.highlighted + headings);
        cx.stop_propagation();
        cx.notify();
    }

    /// `handleCancel` in the bare form: the choice goes back to the base
    /// branch and Merge.
    fn reset_update(&mut self, cx: &mut Context<Self>) {
        if self.update_disabled() {
            return;
        }
        self.update.operation = Some(Integration::Merge);
        self.update.picker = None;
        self.update.selected = self.default_update_target();
        cx.notify();
    }

    /// Done or Close after a run: the log clears and the form returns.
    fn clear_update_run(&mut self, cx: &mut Context<Self>) {
        if !self.update.finished() {
            return;
        }
        self.update.logs.clear();
        self.update.operating = false;
        self.update.selected = self.default_update_target();
        self.update.operation = Some(Integration::Merge);
        cx.notify();
    }

    fn update_disabled(&self) -> bool {
        self.update.operating
            || self.integration_running()
            || self.commit_task.is_some()
            || self.operation.as_ref().is_some_and(|run| {
                run.operation
                    .as_ref()
                    .is_none_or(|operation| operation.state.is_open())
            })
    }

    fn confirm_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update_disabled() {
            return;
        }
        let (Some(target), Some(operation)) = (self.update.selected.clone(), self.update.operation)
        else {
            return;
        };
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let current = self
            .changes()
            .and_then(|changes| changes.branch.clone())
            .unwrap_or_default();
        let directory = self.git_directory();
        let scope = self.scope.clone();
        self.update.operating = true;
        self.update.logs.clear();
        self.update.picker = None;
        self.update.run = Some(cx.spawn_in(window, async move |this, cx| {
            let reference = match &target {
                IntegrationTarget::Local(branch) => branch.clone(),
                IntegrationTarget::Remote(remote) => {
                    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
                    let _ = this.update(cx, |this, cx| {
                        this.update.log(format!("Fetching {}...", remote.label));
                        this.update.fetch_events = Some(cx.spawn(async move |this, cx| {
                            while let Some(id) = received.recv().await {
                                let _ = this.update(cx, |this, cx| {
                                    this.update.fetch_id = Some(id);
                                    cx.notify();
                                });
                            }
                            let _ = this.update(cx, |this, cx| {
                                this.update.fetch_id = None;
                                cx.notify();
                            });
                        }));
                        cx.notify();
                    });
                    let fetch_client = client.clone();
                    let fetch_directory = directory.clone();
                    let fetch_remote = remote.clone();
                    let fetched = scope
                        .run(async move {
                            fetch_remote_branch(
                                &fetch_client,
                                &fetch_directory,
                                &fetch_remote,
                                events,
                            )
                            .await
                        })
                        .await;
                    let failure = match fetched {
                        Some(Ok(sha)) => {
                            let _ = this.update(cx, |this, cx| {
                                this.update.settle(
                                    LogStatus::Done,
                                    Some(format!("Fetched {}", remote.label)),
                                );
                                cx.notify();
                            });
                            Ok(sha)
                        }
                        Some(Err(StepFailure(detail))) => Err(detail),
                        None => Err(String::new()),
                    };
                    match failure {
                        Ok(sha) => sha,
                        Err(detail) => {
                            let _ = this.update(cx, |this, cx| {
                                let strings = locale::strings(cx);
                                let message = strings.format(
                                    "gitView.toast.syncActionFailed",
                                    &[("action", &strings.get("gitView.sync.fetch"))],
                                );
                                if !detail.is_empty() {
                                    tracing::debug!(%detail, "the branch fetch failed");
                                }
                                this.update.settle(LogStatus::Error, Some(message.into()));
                                this.update.run = None;
                                cx.notify();
                            });
                            return;
                        }
                    }
                }
            };
            let label = target.label().to_owned();
            let _ = this.update(cx, |this, cx| {
                this.update.log(match operation {
                    Integration::Merge => format!("Merging {label} into {current}..."),
                    Integration::Rebase => format!("Rebasing {current} onto {label}..."),
                });
                cx.notify();
            });
            let result = scope
                .run(async move { client.integrate(&directory, operation, &reference).await })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.update.run = None;
                match result {
                    Some(Ok(IntegrationOutcome {
                        conflict: true,
                        conflict_files,
                    })) => {
                        this.update.settle(
                            LogStatus::Error,
                            Some(
                                match operation {
                                    Integration::Merge => "Merge conflicts detected",
                                    Integration::Rebase => "Rebase conflicts detected",
                                }
                                .to_owned(),
                            ),
                        );
                        this.open_conflict(operation, conflict_files, window, cx);
                    }
                    Some(Ok(_)) => {
                        this.update.settle(
                            LogStatus::Done,
                            Some(match operation {
                                Integration::Merge => format!("Merged {label} into {current}"),
                                Integration::Rebase => format!("Rebased {current} onto {label}"),
                            }),
                        );
                    }
                    Some(Err(error)) => {
                        let message =
                            error
                                .message()
                                .map(str::to_owned)
                                .unwrap_or_else(|| match operation {
                                    Integration::Merge => format!("Failed to merge {label}"),
                                    Integration::Rebase => format!("Failed to rebase onto {label}"),
                                });
                        this.update.settle(LogStatus::Error, Some(message));
                    }
                    None => this.update.settle(LogStatus::Error, None),
                }
                this.read_branches(cx);
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The header's repository views button.
    fn cancel_update_fetch(&mut self, cx: &mut Context<Self>) {
        if self.update.fetch_cancel.is_some() {
            return;
        }
        let Some(id) = self.update.fetch_id.clone() else {
            return;
        };
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let task = self
            .scope
            .run(async move { client.cancel_operation(&id).await });
        self.update.fetch_cancel = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.update.fetch_cancel = None;
                if !matches!(result, Some(Ok(_))) {
                    let strings = locale::strings(cx);
                    this.notice = Some(crate::files_view::Notice::error(
                        strings.format(
                            "gitView.toast.syncActionFailed",
                            &[("action", &strings.get("gitView.operation.cancel"))],
                        ),
                        None,
                    ));
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn render_repository_views_button(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if self.changes()?.branch.is_none() && !self.in_linked_worktree(cx) {
            return None;
        }
        let strings = locale::strings(cx);
        Some(
            IconButton::new(
                "git-repository-views",
                "more-fill",
                strings.get("gitView.header.repositoryViews"),
            )
            .render(palette, cx, |this: &mut Self, _, cx| {
                this.toggle_repository_views(cx)
            }),
        )
    }

    /// The repository views menu under the header while open.
    pub(super) fn render_repository_views(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.update.menu_open {
            return None;
        }
        let strings = locale::strings(cx);
        let hover = palette.hover;
        let item = on_activate(
            div()
                .id("git-update-branch")
                .role(Role::MenuItem)
                .accessibility_id("git-update-branch")
                .aria_label(strings.get("gitView.header.updateBranch"))
                .h(px(PANEL_ROW))
                .mx(px(4.0))
                .px(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(glyph("git-merge", 14.0, palette.muted_foreground))
                .child(
                    div()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(palette.foreground)
                        .child(strings.get("gitView.header.updateBranch")),
                ),
            cx,
            |this: &mut Self, _, cx| this.open_update_branch(cx),
        )
        .test_support();
        Some(
            div()
                .id("git-repository-views-menu")
                .role(Role::Menu)
                .accessibility_id("git-repository-views-menu")
                .aria_label(strings.get("gitView.header.repositoryViews"))
                .py(px(4.0))
                .when(
                    self.changes()
                        .is_some_and(|changes| changes.branch.is_some()),
                    |menu| menu.child(item),
                )
                .when(self.in_linked_worktree(cx), |menu| {
                    menu.child(text_button(
                        "git-remove-worktree",
                        strings.get("mobile.projectEdit.deleteWorktreeTitle"),
                        None,
                        palette,
                        cx,
                        |this: &mut Self, window, cx| {
                            this.update.menu_open = false;
                            if let Some(owner) =
                                this.scope.owner.as_ref().and_then(|owner| owner.upgrade())
                            {
                                let directory = this.git_directory();
                                owner.update(cx, |owner, cx| {
                                    owner.open_worktree_removal(&directory, window, cx)
                                });
                            }
                        },
                    ))
                })
                .into_any_element(),
        )
    }

    fn in_linked_worktree(&self, cx: &Context<Self>) -> bool {
        self.scope
            .owner
            .as_ref()
            .and_then(|owner| owner.upgrade())
            .is_some_and(|owner| {
                let owner = owner.read(cx);
                let directory = self.git_directory();
                owner.topology.worktrees.iter().any(|group| {
                    group
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == directory)
                })
            })
    }

    /// The dialog over the surface while open.
    pub(super) fn render_update_branch(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.update.open {
            return None;
        }
        let strings = locale::strings(cx);
        let current: SharedString = self
            .changes()
            .and_then(|changes| changes.branch.clone())
            .map(SharedString::from)
            .unwrap_or_default();
        let mono = gpui_kit::component::Theme::global(cx)
            .mono_font_family
            .clone();
        let body = if self.update.operating {
            self.render_update_log(palette, direction, cx)
        } else {
            self.render_update_form(palette, direction, cx)
        };
        Some(
            div()
                .id("git-update-backdrop")
                .absolute()
                .inset_0()
                .bg(palette.background.opacity(0.6))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("git-update-dialog")
                        .role(Role::Dialog)
                        .accessibility_id("git-update-dialog")
                        .aria_label(strings.get("gitView.branch.updateTitle"))
                        .test_support()
                        .w(px(480.0))
                        .max_w_full()
                        .p(px(16.0))
                        .rounded(px(12.0))
                        .bg(palette.elevated)
                        .shadow(elevation::floating(palette))
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .css_text(ROW_SIZE, ROW_LINE, cx)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(palette.foreground)
                                        .child(strings.get("gitView.branch.updateTitle")),
                                )
                                .child(
                                    IconButton::new(
                                        "git-update-close",
                                        "close",
                                        strings.get("gitView.common.close"),
                                    )
                                    .render(
                                        palette,
                                        cx,
                                        |this: &mut Self, _, cx| this.close_update_branch(cx),
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .flex_row_from(direction)
                                .flex_wrap()
                                .gap(px(4.0))
                                .css_text(XS_SIZE, XS_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(strings.get("gitView.branch.updateDescriptionPrefix"))
                                .child(
                                    div()
                                        .font_family(mono)
                                        .text_color(palette.foreground)
                                        .child(SharedString::from(format!("{current}."))),
                                ),
                        )
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn render_update_form(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let operation = self.update.operation.unwrap_or(Integration::Merge);
        let current: String = self
            .changes()
            .and_then(|changes| changes.branch.clone())
            .unwrap_or_else(|| strings.get("gitView.branch.currentBranchFallback").into());
        let tile = |id: &'static str,
                    kind: Integration,
                    icon: &'static str,
                    label: &'static str,
                    description: &'static str| {
            let chosen = operation == kind;
            let hover = palette.hover;
            on_activate(
                div()
                    .id(id)
                    .role(Role::Button)
                    .accessibility_id(id)
                    .aria_label(strings.get(label))
                    .aria_toggled(if chosen {
                        gpui_kit::Toggled::True
                    } else {
                        gpui_kit::Toggled::False
                    })
                    .flex_1()
                    .min_w_0()
                    .p(px(12.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .cursor_pointer()
                    .when(chosen, |this| this.bg(palette.selection))
                    .when(!chosen, |this| {
                        this.bg(palette.muted).hover(move |style| style.bg(hover))
                    })
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(glyph(
                                icon,
                                16.0,
                                if chosen {
                                    palette.selection_foreground
                                } else {
                                    palette.muted_foreground
                                },
                            ))
                            .child(
                                div()
                                    .css_text(ROW_SIZE, ROW_LINE, cx)
                                    .text_color(if chosen {
                                        palette.foreground
                                    } else {
                                        palette.muted_foreground
                                    })
                                    .child(strings.get(label)),
                            ),
                    )
                    .child(
                        div()
                            .id(description)
                            .css_text(XS_SIZE, XS_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.get(description))
                            .test_support(),
                    ),
                cx,
                move |this: &mut Self, _, cx| {
                    this.update.operation = Some(kind);
                    cx.notify();
                },
            )
            .test_support()
        };
        let caption = |text: SharedString| {
            div()
                .css_text(XS_SIZE, XS_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(text)
        };
        let selected_label: SharedString = self
            .update
            .selected
            .as_ref()
            .map(|target| SharedString::from(target.label().to_owned()))
            .unwrap_or_else(|| strings.get("gitView.branch.selectBranch"));
        let hover = palette.hover;
        let trigger = on_activate(
            div()
                .id("git-update-target")
                .role(Role::Button)
                .accessibility_id("git-update-target")
                .when_some(self.update.trigger_focus.as_ref(), |this, focus| {
                    this.track_focus(focus)
                })
                .aria_label(selected_label.clone())
                .aria_expanded(self.update.picker.is_some())
                .h(px(32.0))
                .px(px(10.0))
                .rounded(px(CONTROL_RADIUS))
                .bg(palette.background)
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(if self.update.selected.is_some() {
                            palette.foreground
                        } else {
                            palette.muted_foreground
                        })
                        .child(selected_label),
                )
                .child(glyph("arrow-down-s", 16.0, palette.muted_foreground)),
            cx,
            |this: &mut Self, window, cx| this.toggle_picker(window, cx),
        )
        .test_support();
        let summary = self.update.selected.as_ref().map(|target| {
            let mono = gpui_kit::component::Theme::global(cx)
                .mono_font_family
                .clone();
            let code = |text: String| {
                div()
                    .font_family(mono.clone())
                    .text_color(palette.foreground)
                    .child(SharedString::from(text))
            };
            let (prefix, first, infix, second) = match operation {
                Integration::Merge => (
                    "gitView.branch.summaryMergePrefix",
                    target.label().to_owned(),
                    "gitView.branch.summaryMergeInfix",
                    current.clone(),
                ),
                Integration::Rebase => (
                    "gitView.branch.summaryRebasePrefix",
                    current.clone(),
                    "gitView.branch.summaryRebaseInfix",
                    target.label().to_owned(),
                ),
            };
            div()
                .id("git-update-summary")
                .role(Role::Status)
                .accessibility_id("git-update-summary")
                .aria_label(SharedString::from(format!(
                    "{} {first} {} {second}",
                    strings.get(prefix),
                    strings.get(infix)
                )))
                .test_support()
                .p(px(12.0))
                .rounded(px(8.0))
                .bg(palette.muted)
                .flex_row_from(direction)
                .flex_wrap()
                .gap(px(4.0))
                .css_text(XS_SIZE, XS_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(strings.get(prefix))
                .child(code(first))
                .child(strings.get(infix))
                .child(code(second))
        });
        let confirm_label = strings.get(match operation {
            Integration::Merge => "gitView.operation.merge",
            Integration::Rebase => "gitView.operation.rebase",
        });
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(caption(strings.get("gitView.branch.operation")))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .gap(px(12.0))
                            .child(tile(
                                "git-update-merge",
                                Integration::Merge,
                                "git-merge",
                                "gitView.operation.merge",
                                "gitView.branch.mergeDescription",
                            ))
                            .child(tile(
                                "git-update-rebase",
                                Integration::Rebase,
                                "git-branch",
                                "gitView.operation.rebase",
                                "gitView.branch.rebaseDescription",
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(caption(match operation {
                        Integration::Merge => strings
                            .format("gitView.branch.branchToMergeInto", &[("branch", &current)]),
                        Integration::Rebase => strings.get("gitView.branch.branchToRebaseOnto"),
                    }))
                    .child(trigger)
                    .children(self.render_update_picker(palette, direction, cx)),
            )
            .children(summary)
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(text_button(
                        "git-update-reset",
                        strings.get("gitView.common.reset"),
                        None,
                        palette,
                        cx,
                        |this: &mut Self, _, cx| this.reset_update(cx),
                    ))
                    .child(div().flex_1())
                    .child(text_button(
                        "git-update-confirm",
                        confirm_label,
                        None,
                        palette,
                        cx,
                        |this: &mut Self, window, cx| this.confirm_update(window, cx),
                    )),
            )
            .into_any_element()
    }

    fn render_update_picker(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let input = self.update.picker.as_ref()?;
        let strings = locale::strings(cx);
        let query = input.read(cx).value().to_string();
        let local = self
            .branches
            .as_ref()
            .map(|branches| branches.local.clone())
            .unwrap_or_default();
        let remote = self.remote_targets();
        let current = self.changes().and_then(|changes| changes.branch.clone());
        let (local, remote) = filter_targets(&local, &remote, current.as_deref(), &query);
        let hover = palette.hover;
        let highlighted = self
            .picker_targets(cx)
            .get(self.update.highlighted)
            .cloned();
        let row = |target: IntegrationTarget| {
            let label = SharedString::from(target.label().to_owned());
            let id = SharedString::from(format!("git-update-target:{label}"));
            on_activate(
                div()
                    .id(id.clone())
                    .role(Role::ListBoxOption)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .aria_selected(highlighted.as_ref() == Some(&target))
                    .when(highlighted.as_ref() == Some(&target), |row| {
                        row.bg(palette.selection)
                    })
                    .h(px(PANEL_ROW))
                    .px(px(8.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex_row_from(direction)
                    .items_center()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(
                        div()
                            .truncate()
                            .css_text(ROW_SIZE, ROW_LINE, cx)
                            .text_color(palette.foreground)
                            .child(label),
                    ),
                cx,
                move |this: &mut Self, _, cx| this.choose_target(target.clone(), cx),
            )
            .test_support()
            .into_any_element()
        };
        let heading = |key: &str| {
            div()
                .px(px(8.0))
                .pt(px(4.0))
                .css_text(XS_SIZE, XS_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(strings.get(key))
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        if !local.is_empty() {
            rows.push(heading("gitView.branch.localBranches"));
            rows.extend(
                local
                    .into_iter()
                    .map(|branch| row(IntegrationTarget::Local(branch.clone()))),
            );
        }
        if !remote.is_empty() {
            rows.push(heading("gitView.branch.remoteBranches"));
            rows.extend(
                remote
                    .into_iter()
                    .map(|target| row(IntegrationTarget::Remote(target.clone()))),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(strings.get("gitView.branch.empty"))
                    .into_any_element(),
            );
        }
        Some(
            div()
                .id("git-update-targets")
                .key_context(super::UPDATE_PICKER_CONTEXT)
                .role(Role::ListBox)
                .accessibility_id("git-update-targets")
                .aria_label(strings.get("gitView.branch.selectBranch"))
                .test_support()
                .rounded(px(8.0))
                .bg(palette.background)
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .capture_key_down(cx.listener(Self::update_picker_key))
                .child(
                    div()
                        .id("git-update-search")
                        .role(Role::Group)
                        .accessibility_id("git-update-search")
                        .aria_label(strings.get("gitView.branch.searchPlaceholder"))
                        .h(px(28.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .child(
                            Input::new(input)
                                .appearance(false)
                                .bordered(false)
                                .text_size(gpui_kit::rems(ROW_SIZE)),
                        ),
                )
                .child(
                    div()
                        .id("git-update-target-list")
                        .track_scroll(&self.update.picker_scroll)
                        .max_h(px(200.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    fn render_update_log(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let failed = self
            .update
            .logs
            .iter()
            .any(|log| log.status == LogStatus::Error);
        let entries = self.update.logs.iter().enumerate().map(|(index, log)| {
            let (icon, color) = match log.status {
                LogStatus::Running => ("loader-4", palette.primary),
                LogStatus::Done => ("check", palette.status_success),
                LogStatus::Error => ("close", palette.status_error),
            };
            div()
                .id(SharedString::from(format!("git-update-log:{index}")))
                .role(Role::ListItem)
                .aria_label(log.message.clone())
                .flex_row_from(direction)
                .items_start()
                .gap(px(8.0))
                .child(glyph(icon, 14.0, color))
                .child(
                    div()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(match log.status {
                            LogStatus::Error => palette.status_error,
                            LogStatus::Done => palette.muted_foreground,
                            LogStatus::Running => palette.foreground,
                        })
                        .child(log.message.clone()),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .id("git-update-log")
                    .role(Role::List)
                    .accessibility_id("git-update-log")
                    .aria_label(strings.get("gitView.branch.updateTitle"))
                    .test_support()
                    .max_h(px(192.0))
                    .overflow_y_scroll()
                    .p(px(12.0))
                    .rounded(px(8.0))
                    .bg(palette.muted)
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(entries),
            )
            .when(self.update.fetch_id.is_some(), |body| {
                body.child(text_button(
                    "git-update-cancel-fetch",
                    strings.get("gitView.operation.cancel"),
                    None,
                    palette,
                    cx,
                    |this: &mut Self, _, cx| this.cancel_update_fetch(cx),
                ))
            })
            .when(self.update.finished(), |this| {
                this.child(
                    div()
                        .flex_row_from(direction)
                        .justify_end()
                        .child(text_button(
                            "git-update-done",
                            strings.get(if failed {
                                "gitView.common.close"
                            } else {
                                "gitView.common.done"
                            }),
                            None,
                            palette,
                            cx,
                            |this: &mut Self, _, cx| this.clear_update_run(cx),
                        )),
                )
            })
            .into_any_element()
    }
}

/// Plans and runs the fetch of `target`'s one branch, then reads the commit
/// it left in the tracking ref (`resolveBranchIntegrationRef`).
async fn fetch_remote_branch(
    client: &openchamber_native_transport::client::ServerClient,
    directory: &str,
    target: &RemoteTarget,
    events: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<String, StepFailure> {
    let failure = |error: &dyn std::fmt::Display| StepFailure(error.to_string());
    let binding = client
        .repository_binding(directory)
        .await
        .map_err(|error| failure(&error))?;
    let request = ref_fetch_request(directory, &binding, &target.remote, &target.branch)
        .map_err(|refusal| StepFailure(format!("{refusal:?}")))?;
    let planned = client
        .plan_operation(&request)
        .await
        .map_err(|error| failure(&error))?;
    let _ = events.send(planned.id.clone());
    let mut observed = client.execute_operation(&planned.id).await.ok();
    let finished = loop {
        if let Some(operation) = observed.take()
            && !operation.state.is_open()
        {
            break operation;
        }
        // A timeout or unavailable read grants no terminal outcome. Keep the
        // original plan alive and cancellable until that same ID settles.
        tokio::time::sleep(super::OPERATION_POLL).await;
        observed = client.read_operation(&planned.id).await.ok();
    };
    if finished.state != OperationState::Succeeded {
        return Err(StepFailure(format!("{:?}", finished.state)));
    }
    let sha = client
        .latest_commit(directory, &target.destination_ref)
        .await
        .map_err(|error| failure(&error))?
        .unwrap_or_default();
    if !branch_integration::is_commit_hash(&sha) {
        return Err(StepFailure("the fetched ref has no commit".to_owned()));
    }
    Ok(sha)
}
