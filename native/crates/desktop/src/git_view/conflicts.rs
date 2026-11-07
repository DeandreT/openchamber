//! Merges, rebases and their conflicts on the Git surface
//! (`InProgressOperationBanner.tsx`, `ConflictDialog.tsx` and the branch
//! integration in `GitView.tsx`).
//!
//! A branch row merges that branch into the current one or rebases onto it.
//! A run that stops on conflicts opens the conflict dialog, which reads the
//! server's conflict details. While the status reports a merge or rebase in
//! progress the banner offers Abort and, once no path is conflicted,
//! Continue. Every one of these ends in a status read, as index mutations do.

use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, Task, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::git_changes::{GitChanges, Integration};
use openchamber_native_transport::git::{ConflictDetails, IntegrationOutcome};

use super::GitView;
use crate::baseline::CssTextExt;
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::elevation;
use crate::files_view::{Notice, text_button};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

/// The surface's integration state.
#[derive(Default)]
pub(super) struct Conflicts {
    dialog: Option<ConflictDialog>,
    /// A merge, rebase, continue or abort is running.
    running: Option<Task<()>>,
}

struct ConflictDialog {
    operation: Integration,
    /// The paths the run reported, until the details list them.
    files: Vec<String>,
    /// `None` while reading; the error's text when the read failed.
    details: Option<Result<ConflictDetails, String>>,
    _read: Task<()>,
}

fn operation_label(operation: Integration, strings: &Strings) -> SharedString {
    strings.get(match operation {
        Integration::Merge => "gitView.operation.merge",
        Integration::Rebase => "gitView.operation.rebase",
    })
}

impl GitView {
    pub(super) fn integration_running(&self) -> bool {
        self.conflicts.running.is_some()
    }

    /// Merges `branch` into the current branch or rebases onto it.
    pub(super) fn integrate(
        &mut self,
        operation: Integration,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.conflicts.running.is_some() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let current = self
            .changes()
            .and_then(|changes| changes.branch.clone())
            .unwrap_or_default();
        let directory = self.scope.directory.clone();
        let target = branch.clone();
        let task = self
            .scope
            .run(async move { client.integrate(&directory, operation, &target).await });
        self.conflicts.running = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.conflicts.running = None;
                let strings = locale::strings(cx).clone();
                match result {
                    Some(Ok(IntegrationOutcome {
                        conflict: true,
                        conflict_files,
                    })) => {
                        this.notice = Some(Notice::error(
                            strings.get(match operation {
                                Integration::Merge => "gitView.toast.mergeConflictsDetected",
                                Integration::Rebase => "gitView.toast.rebaseConflictsDetected",
                            }),
                            None,
                        ));
                        this.open_conflict(operation, conflict_files, window, cx);
                    }
                    Some(Ok(_)) => {
                        let text = match operation {
                            Integration::Merge => strings.format(
                                "gitView.toast.mergedIntoBranch",
                                &[("branch", &branch), ("currentBranch", &current)],
                            ),
                            Integration::Rebase => strings.format(
                                "gitView.toast.rebasedOntoBranch",
                                &[("branch", &branch), ("currentBranch", &current)],
                            ),
                        };
                        this.notice = Some(Notice::info(text));
                    }
                    Some(Err(error)) => {
                        this.notice = Some(Notice::error(
                            strings.get(match operation {
                                Integration::Merge => "gitView.toast.mergeConflictsDetected",
                                Integration::Rebase => "gitView.toast.rebaseConflictsDetected",
                            }),
                            error.message(),
                        ));
                    }
                    None => {}
                }
                this.read_branches(cx);
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Opens the dialog and reads the conflict details for it.
    fn open_conflict(
        &mut self,
        operation: Integration,
        files: Vec<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.scope.directory.clone();
        let task = self
            .scope
            .run(async move { client.conflict_details(&directory).await });
        let read = cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let strings = locale::strings(cx);
                if let Some(dialog) = this.conflicts.dialog.as_mut() {
                    dialog.details = Some(result.map_err(|error| {
                        error
                            .message()
                            .map(str::to_owned)
                            .unwrap_or_else(|| strings.get("gitView.conflict.loadFailed").into())
                    }));
                }
                cx.notify();
            });
        });
        self.conflicts.dialog = Some(ConflictDialog {
            operation,
            files,
            details: None,
            _read: read,
        });
        cx.notify();
    }

    /// The banner's Continue: a further conflict reopens the dialog.
    fn continue_integration(
        &mut self,
        operation: Integration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_step(operation, true, window, cx);
    }

    fn abort_integration(
        &mut self,
        operation: Integration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.conflicts.dialog = None;
        self.run_step(operation, false, window, cx);
    }

    fn run_step(
        &mut self,
        operation: Integration,
        resume: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.conflicts.running.is_some() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.scope.directory.clone();
        let task = self.scope.run(async move {
            if resume {
                client.continue_integration(&directory, operation).await
            } else {
                client
                    .abort_integration(&directory, operation)
                    .await
                    .map(|()| IntegrationOutcome::default())
            }
        });
        self.conflicts.running = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.conflicts.running = None;
                let strings = locale::strings(cx).clone();
                let merge = operation == Integration::Merge;
                match result {
                    Some(Ok(outcome)) if outcome.conflict => {
                        this.notice = Some(Notice::error(
                            strings.get(if merge {
                                "gitView.toast.mergeConflictsDetected"
                            } else {
                                "gitView.toast.rebaseConflictsDetected"
                            }),
                            None,
                        ));
                        this.open_conflict(operation, outcome.conflict_files, window, cx);
                    }
                    Some(Ok(_)) => {
                        let key = match (resume, merge) {
                            (true, true) => "gitView.toast.mergeCompleted",
                            (true, false) => "gitView.toast.rebaseStepCompleted",
                            (false, true) => "gitView.toast.mergeAborted",
                            (false, false) => "gitView.toast.rebaseAborted",
                        };
                        this.notice = Some(Notice::info(strings.get(key)));
                    }
                    Some(Err(error)) => {
                        this.notice = Some(Notice::error(
                            strings.get(if resume {
                                "gitView.toast.continueOperationFailed"
                            } else {
                                "gitView.toast.abortOperationFailed"
                            }),
                            error.message(),
                        ));
                    }
                    None => {}
                }
                this.read_branches(cx);
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The banner while a merge or rebase is in progress.
    pub(super) fn render_in_progress(
        &self,
        changes: Option<&GitChanges>,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let changes = changes?;
        let operation = changes.in_progress()?;
        let strings = locale::strings(cx);
        let label = operation_label(operation, strings);
        let conflicts = changes.conflicted().len();
        let count = conflicts.to_string();
        let title = match conflicts {
            0 => strings.format(
                "gitView.operation.inProgressTitle",
                &[("operation", &label)],
            ),
            1 => strings.format(
                "gitView.operation.inProgressTitleOneConflict",
                &[("operation", &label), ("count", &count)],
            ),
            _ => strings.format(
                "gitView.operation.inProgressTitleManyConflicts",
                &[("operation", &label), ("count", &count)],
            ),
        };
        let description = match operation {
            Integration::Merge => changes.merge.as_ref().map(|merge| match &merge.message {
                Some(message) => {
                    strings.format("gitView.operation.mergingMessage", &[("message", message)])
                }
                None => strings.format(
                    "gitView.operation.mergeInProgressWithHead",
                    &[("head", &merge.head)],
                ),
            }),
            Integration::Rebase => changes
                .rebase
                .as_ref()
                .map(|rebase| match &rebase.head_name {
                    Some(head) => strings.format(
                        "gitView.operation.rebasingOnto",
                        &[
                            ("headName", head),
                            ("onto", rebase.onto.as_deref().unwrap_or_default()),
                        ],
                    ),
                    None => strings.get("gitView.operation.rebaseInProgress"),
                }),
        };
        let hint = strings.get(if conflicts > 0 {
            "gitView.operation.resolveConflictsHint"
        } else {
            "gitView.operation.readyToContinueHint"
        });
        let busy = self.integration_running();
        let abort = text_button(
            "git-operation-abort",
            strings.get("gitView.operation.abort"),
            None,
            palette,
            cx,
            move |this: &mut Self, window, cx| {
                if !busy {
                    this.abort_integration(operation, window, cx)
                }
            },
        );
        let primary = if conflicts > 0 {
            let files = changes.conflicted();
            text_button(
                "git-operation-resolve",
                strings.get("gitView.operation.resolveWithAi"),
                None,
                palette,
                cx,
                move |this: &mut Self, window, cx| {
                    this.open_conflict(operation, files.clone(), window, cx)
                },
            )
        } else {
            text_button(
                "git-operation-continue",
                strings.get("gitView.operation.continue"),
                None,
                palette,
                cx,
                move |this: &mut Self, window, cx| {
                    if !busy {
                        this.continue_integration(operation, window, cx)
                    }
                },
            )
        };
        Some(
            div()
                .id("git-in-progress")
                .role(Role::Status)
                .accessibility_id("git-in-progress")
                .aria_label(title.clone())
                .mx(px(12.0))
                .mt(px(8.0))
                .p(px(12.0))
                .rounded(px(8.0))
                .bg(palette.status_warning_background)
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.status_warning)
                        .child(title),
                )
                .children(description.map(|text| {
                    div()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(text)
                }))
                .child(
                    div()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .css_text(XS_SIZE, XS_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(hint),
                        )
                        .child(abort)
                        .child(primary),
                )
                .test_support()
                .into_any_element(),
        )
    }

    /// The conflict dialog over the surface.
    pub(super) fn render_conflict_dialog(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.conflicts.dialog.as_ref()?;
        let strings = locale::strings(cx);
        let operation = dialog.operation;
        let label = operation_label(operation, strings);
        let word = match operation {
            Integration::Merge => "merge",
            Integration::Rebase => "rebase",
        };
        let details = dialog
            .details
            .as_ref()
            .and_then(|details| details.as_ref().ok());
        let files: Vec<String> = details
            .map(|details| details.unmerged_files.clone())
            .filter(|files| !files.is_empty())
            .unwrap_or_else(|| dialog.files.clone());
        let mono = gpui_kit::component::Theme::global(cx)
            .mono_font_family
            .clone();
        let muted_box = |child: AnyElement| {
            div()
                .p(px(8.0))
                .rounded(px(8.0))
                .bg(palette.muted)
                .child(child)
        };
        let not_ported = |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
            // Handing the conflict to a session needs the composer's
            // synthetic parts, which are not ported yet.
            tracing::info!("resolving a conflict in a session is not ported");
            cx.notify();
            let _ = this;
        };
        Some(
            div()
                .id("git-conflict-backdrop")
                .absolute()
                .inset_0()
                .bg(palette.background.opacity(0.6))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("git-conflict")
                        .role(Role::Dialog)
                        .accessibility_id("git-conflict")
                        .aria_label(
                            strings
                                .format("gitView.conflict.detectedTitle", &[("operation", &label)]),
                        )
                        .test_support()
                        .w(px(400.0))
                        .p(px(16.0))
                        .rounded(px(12.0))
                        .bg(palette.elevated)
                        .shadow(elevation::floating(palette))
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(
                            div()
                                .css_text(ROW_SIZE, ROW_LINE, cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(palette.foreground)
                                .child(strings.format(
                                    "gitView.conflict.detectedTitle",
                                    &[("operation", &label)],
                                )),
                        )
                        .child(
                            div()
                                .css_text(ROW_SIZE, ROW_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(strings.format(
                                    "gitView.conflict.detectedDescription",
                                    &[("operation", word)],
                                )),
                        )
                        .when(dialog.details.is_none(), |this| {
                            this.child(
                                div()
                                    .css_text(XS_SIZE, XS_LINE, cx)
                                    .text_color(palette.muted_foreground)
                                    .child(strings.get("gitView.conflict.loading")),
                            )
                        })
                        .when_some(
                            dialog
                                .details
                                .as_ref()
                                .and_then(|details| details.as_ref().err()),
                            |this, message| {
                                this.child(
                                    div()
                                        .p(px(8.0))
                                        .rounded(px(8.0))
                                        .bg(palette.status_error.opacity(0.15))
                                        .css_text(XS_SIZE, XS_LINE, cx)
                                        .text_color(palette.status_error)
                                        .child(strings.format(
                                            "gitView.conflict.errorLoadingDetails",
                                            &[("message", message)],
                                        )),
                                )
                            },
                        )
                        .when(!files.is_empty(), |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(
                                        div()
                                            .flex_row_from(direction)
                                            .justify_between()
                                            .css_text(XS_SIZE, XS_LINE, cx)
                                            .text_color(palette.muted_foreground)
                                            .child(strings.get("gitView.conflict.conflictedFiles"))
                                            .child(files.len().to_string()),
                                    )
                                    .child(muted_box(
                                        div()
                                            .id("git-conflict-files")
                                            .role(Role::List)
                                            .accessibility_id("git-conflict-files")
                                            .max_h(px(160.0))
                                            .overflow_y_scroll()
                                            .flex()
                                            .flex_col()
                                            .children(files.iter().map(|file| {
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "git-conflict-file:{file}"
                                                    )))
                                                    .role(Role::ListItem)
                                                    .aria_label(SharedString::from(file.clone()))
                                                    .truncate()
                                                    .font_family(mono.clone())
                                                    .text_size(gpui_kit::rems(0.6875))
                                                    .text_color(palette.foreground)
                                                    .child(file.clone())
                                            }))
                                            .into_any_element(),
                                    )),
                            )
                        })
                        .when_some(
                            details
                                .map(|details| details.head_info.clone())
                                .filter(|head| !head.is_empty()),
                            |this, head| {
                                this.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(4.0))
                                        .child(
                                            div()
                                                .css_text(XS_SIZE, XS_LINE, cx)
                                                .text_color(palette.muted_foreground)
                                                .child(strings.get("gitView.conflict.headInfo")),
                                        )
                                        .child(muted_box(
                                            div()
                                                .font_family(mono.clone())
                                                .text_size(gpui_kit::rems(0.6875))
                                                .text_color(palette.foreground)
                                                .child(head)
                                                .into_any_element(),
                                        )),
                                )
                            },
                        )
                        .child(text_button(
                            "git-conflict-new-session",
                            strings.get("gitView.conflict.resolveNewSession"),
                            None,
                            palette,
                            cx,
                            not_ported,
                        ))
                        .child(text_button(
                            "git-conflict-current-session",
                            strings.get("gitView.conflict.resolveCurrentSession"),
                            None,
                            palette,
                            cx,
                            not_ported,
                        ))
                        .child(
                            div()
                                .flex_row_from(direction)
                                .justify_end()
                                .gap(px(6.0))
                                .child(text_button(
                                    "git-conflict-later",
                                    strings.get("gitView.conflict.continueLater"),
                                    None,
                                    palette,
                                    cx,
                                    |this: &mut Self, _, cx| {
                                        this.conflicts.dialog = None;
                                        cx.notify();
                                    },
                                ))
                                .child(text_button(
                                    "git-conflict-abort",
                                    // `gitView.conflict.abortOperation` drops its
                                    // placeholder in some locales, which the
                                    // native resources refuse.
                                    strings.get("gitView.operation.abort"),
                                    None,
                                    palette,
                                    cx,
                                    move |this: &mut Self, window, cx| {
                                        this.abort_integration(operation, window, cx)
                                    },
                                )),
                        ),
                )
                .into_any_element(),
        )
    }
}
