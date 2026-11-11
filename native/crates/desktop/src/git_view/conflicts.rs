//! Merges, rebases and their conflicts on the Git surface
//! (`InProgressOperationBanner.tsx`, `ConflictDialog.tsx` and the branch
//! integration in `GitView.tsx`).
//!
//! A merge or rebase from the Update branch dialog (`update_branch.rs`) that
//! stops on conflicts opens the conflict dialog, which reads the
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

use std::collections::HashMap;

use openchamber_native_core::magic_prompt::MagicPrompts;
use serde::Serialize;

use super::GitView;
use crate::baseline::CssTextExt;
use crate::composer_handoff::{HandoffRefusal, HandoffTarget};
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
    /// The prompts for a hand-off to a session are being read.
    handoff: Option<Task<()>>,
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

    /// Opens the dialog and reads the conflict details for it.
    pub(super) fn open_conflict(
        &mut self,
        operation: Integration,
        files: Vec<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
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
        let directory = self.git_directory();
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

    /// Hands the conflict to a session (`handleResolveInNewSession` and
    /// `handleResolveInCurrentSession`): the visible prompt goes into the
    /// composer and the instructions and the conflict's JSON go hidden
    /// beside it. A new session opens in the surface's project.
    fn resolve_in_session(
        &mut self,
        target: HandoffTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.conflicts.dialog.as_ref() else {
            return;
        };
        let strings = locale::strings(cx).clone();
        let Some(Ok(details)) = dialog.details.clone() else {
            self.notice = Some(Notice::error(
                strings.get("gitView.conflict.noDetailsAvailable"),
                None,
            ));
            cx.notify();
            return;
        };
        let operation = dialog.operation;
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let target = match target {
            HandoffTarget::NewSession(_) => HandoffTarget::NewSession(self.scope.directory.clone()),
            current => current,
        };
        let read_directory = directory.clone();
        let runtime = self.scope.runtime.clone();
        let Some(owner) = self.scope.owner.as_ref().and_then(|owner| owner.upgrade()) else {
            return;
        };
        let navigation = owner.read(cx).navigation_revision;
        let overrides = self
            .scope
            .run(async move { client.magic_prompt_overrides(Some(&read_directory)).await });
        self.conflicts.handoff = Some(cx.spawn_in(window, async move |this, cx| {
            let overrides = overrides.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.conflicts.handoff = None;
                let overrides = match overrides {
                    Some(Ok(overrides)) => overrides,
                    Some(Err(error)) => {
                        this.notice = Some(Notice::error(
                            strings.get("magicPrompts.toast.loadFailed"),
                            Some(&error.to_string()),
                        ));
                        cx.notify();
                        return;
                    }
                    None => return,
                };
                let prompts = MagicPrompts {
                    overrides: &overrides,
                    defaults: crate::starters::MagicCatalog::get().templates(),
                };
                let handoff = conflict_handoff(&details, &directory, operation, &prompts);
                let Some(workspace) = this.scope.owner.as_ref().and_then(|owner| owner.upgrade())
                else {
                    return;
                };
                let placed = workspace.update(cx, |workspace, cx| {
                    if workspace.runtime != runtime || workspace.navigation_revision != navigation {
                        return Err(HandoffRefusal::Unavailable);
                    }
                    workspace.hand_off_to_composer(
                        target,
                        handoff.visible,
                        vec![handoff.instructions, handoff.payload],
                        window,
                        cx,
                    )
                });
                match placed {
                    Ok(()) => this.conflicts.dialog = None,
                    Err(HandoffRefusal::NoActiveSession) => {
                        this.notice = Some(Notice::error(
                            strings.get("gitView.conflict.noActiveSession"),
                            Some(&strings.get("gitView.conflict.noActiveSessionDescription")),
                        ));
                    }
                    Err(HandoffRefusal::UnknownProject) => {}
                    Err(HandoffRefusal::Unavailable) => {
                        this.notice = Some(Notice::error(
                            strings.get("chat.nativeRecovery.storageFailed"),
                            None,
                        ));
                    }
                }
                cx.notify();
            });
        }));
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
                                                let id = SharedString::from(format!(
                                                    "git-conflict-file:{file}"
                                                ));
                                                div()
                                                    .id(id.clone())
                                                    .role(Role::ListItem)
                                                    .accessibility_id(id)
                                                    .test_support()
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
                            |this: &mut Self, window, cx| {
                                this.resolve_in_session(
                                    HandoffTarget::NewSession(String::new()),
                                    window,
                                    cx,
                                )
                            },
                        ))
                        .child(text_button(
                            "git-conflict-current-session",
                            strings.get("gitView.conflict.resolveCurrentSession"),
                            None,
                            palette,
                            cx,
                            |this: &mut Self, window, cx| {
                                this.resolve_in_session(HandoffTarget::CurrentSession, window, cx)
                            },
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

/// The three texts a conflict's hand-off carries (`buildConflictContext`).
struct ConflictHandoff {
    visible: String,
    instructions: String,
    payload: String,
}

/// The conflict's state as the hidden JSON part lists it, in the reference's
/// key order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConflictPayload<'a> {
    directory: &'a str,
    operation: Option<&'static str>,
    head_info: &'a str,
    status_porcelain: &'a str,
    unmerged_files: &'a [String],
    diff: &'a str,
}

fn operation_word(operation: Integration) -> &'static str {
    match operation {
        Integration::Merge => "merge",
        Integration::Rebase => "rebase",
    }
}

fn conflict_handoff(
    details: &ConflictDetails,
    directory: &str,
    operation: Integration,
    prompts: &MagicPrompts<'_>,
) -> ConflictHandoff {
    let label = operation_word(operation);
    let head_ref = if details.head_info.is_empty() {
        match operation {
            Integration::Merge => "MERGE_HEAD",
            Integration::Rebase => "REBASE_HEAD",
        }
    } else {
        details.head_info.as_str()
    };
    let continue_command = match operation {
        Integration::Merge => "git commit --no-edit",
        Integration::Rebase => "git rebase --continue",
    };
    let variables = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    };
    let visible = prompts
        .render(
            "git.conflict.resolve.visible",
            &variables(&[("operation_label", label), ("head_ref", head_ref)]),
        )
        .unwrap_or_default();
    let head_info = if details.head_info.is_empty() {
        "N/A"
    } else {
        details.head_info.as_str()
    };
    let instructions = prompts
        .render(
            "git.conflict.resolve.instructions",
            &variables(&[
                ("operation_label", label),
                ("directory", directory),
                ("operation", label),
                ("head_info", head_info),
                ("continue_cmd", continue_command),
            ]),
        )
        .unwrap_or_default();
    let payload = ConflictPayload {
        directory,
        operation: details.operation.map(operation_word),
        head_info: &details.head_info,
        status_porcelain: &details.status_porcelain,
        unmerged_files: &details.unmerged_files,
        diff: &details.diff,
    };
    let json = serde_json::to_string_pretty(&payload).expect("the conflict payload serializes");
    ConflictHandoff {
        visible,
        instructions,
        payload: format!("{label} conflict context (JSON)\n{json}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handoff_renders_the_prompts_and_the_reference_json() {
        let details = ConflictDetails {
            operation: Some(Integration::Merge),
            head_info: String::new(),
            status_porcelain: "UU a.txt".to_owned(),
            unmerged_files: vec!["a.txt".to_owned()],
            diff: "diff".to_owned(),
        };
        let defaults = HashMap::from([
            (
                "git.conflict.resolve.visible".to_owned(),
                "Fix the {{operation_label}} from {{head_ref}}".to_owned(),
            ),
            (
                "git.conflict.resolve.instructions".to_owned(),
                "{{directory}} {{head_info}} {{continue_cmd}}".to_owned(),
            ),
        ]);
        let overrides = HashMap::new();
        let prompts = MagicPrompts {
            overrides: &overrides,
            defaults: &defaults,
        };
        let handoff = conflict_handoff(&details, "/repo", Integration::Merge, &prompts);
        assert_eq!(handoff.visible, "Fix the merge from MERGE_HEAD");
        assert_eq!(handoff.instructions, "/repo N/A git commit --no-edit");
        assert_eq!(
            handoff.payload,
            "merge conflict context (JSON)\n{\n  \"directory\": \"/repo\",\n  \"operation\": \"merge\",\n  \"headInfo\": \"\",\n  \"statusPorcelain\": \"UU a.txt\",\n  \"unmergedFiles\": [\n    \"a.txt\"\n  ],\n  \"diff\": \"diff\"\n}",
            "the JSON matches JSON.stringify(value, null, 2)"
        );
    }
}
