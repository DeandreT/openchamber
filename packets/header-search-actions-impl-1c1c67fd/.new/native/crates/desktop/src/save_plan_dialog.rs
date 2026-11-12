//! Save as plan from an answer's More menu (`SaveProjectPlanDialog.tsx` and
//! `handleConfirmSaveAsPlan` in `MessageBody.tsx`).
//!
//! The dialog proposes a title from the answer's first sentence and shows
//! the text it will save. Save creates a plan in the knowledge of the
//! project, or the Chats root, the session belongs to; open knowledge panels
//! of that owner read again. A failed save keeps the dialog and its title.

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AppContext, Context, Entity, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, prelude::FluentBuilder, px,
    rems,
};
use openchamber_native_core::ids::{RuntimeId, SessionId};

use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::session_export::dialog_button;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;
use crate::workspace::Workspace;

/// `typography-ui-label`.
const LABEL_SIZE: f32 = 0.843_75;
/// `max-w-lg`.
const DIALOG_WIDTH: f32 = 512.0;
/// `max-h-40`.
const PREVIEW_MAX_HEIGHT: f32 = 160.0;
/// The longest title the suggestion proposes.
const TITLE_LIMIT: usize = 160;
const FALLBACK_TITLE: &str = "Plan";

/// A plan waiting for its title in the open dialog.
pub(crate) struct SavePlan {
    runtime: RuntimeId,
    session: SessionId,
    /// The knowledge owner the plan is created in.
    project_id: String,
    /// The answer's text, saved as the plan's body.
    body: String,
    pub(crate) title: Entity<InputState>,
    pub(crate) saving: bool,
}

/// `suggestPlanTitleFromText`: the first sentence of the text's first
/// non-empty line, without its heading, bullet or number marker.
pub(crate) fn suggest_plan_title(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let line = normalized
        .split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(FALLBACK_TITLE);
    let line = match line.strip_prefix('#') {
        Some(rest) => rest.trim_start_matches('#').trim_start(),
        None => line,
    };
    let line = strip_marker(line, |rest| rest.strip_prefix(['-', '*', '+']));
    let line = strip_marker(line, |rest| {
        let digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        (digits.len() < rest.len())
            .then(|| digits.strip_prefix('.'))
            .flatten()
    });
    // The shortest start that ends a sentence before a space or the end.
    let mut sentence = line;
    let mut characters = line.char_indices().peekable();
    // A sentence has a character before its mark.
    characters.next();
    while let Some((at, character)) = characters.next() {
        let ends = matches!(character, '.' | '!' | '?')
            && characters
                .peek()
                .is_none_or(|(_, next)| next.is_whitespace());
        if ends {
            sentence = &line[..at + character.len_utf8()];
            break;
        }
    }
    let compact = sentence.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > TITLE_LIMIT {
        let cut: String = compact.chars().take(TITLE_LIMIT).collect();
        return cut.trim().to_owned();
    }
    if compact.is_empty() {
        FALLBACK_TITLE.to_owned()
    } else {
        compact
    }
}

/// `line` without the marker `marker` strips, when whitespace follows it.
fn strip_marker<'a>(line: &'a str, marker: impl Fn(&'a str) -> Option<&'a str>) -> &'a str {
    match marker(line) {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => line,
    }
}

impl Workspace {
    /// Whether Save as plan can run for `session`: its folder belongs to a
    /// project or to Chats (`currentProjectRef`).
    pub(crate) fn can_save_plan(&self, session: &SessionId, cx: &gpui_kit::App) -> bool {
        self.session_actions_available() && self.plan_owner(session, cx).is_some()
    }

    fn plan_owner(&self, session: &SessionId, cx: &gpui_kit::App) -> Option<String> {
        let directory = self.session(session)?.resolved_directory()?;
        self.knowledge_owner(&directory, cx)
            .map(|owner| owner.project_id)
    }

    /// Opens the dialog for an answer's text. Empty text opens nothing; a
    /// session outside every project says so.
    pub(crate) fn open_save_plan_dialog(
        &mut self,
        session: &SessionId,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        let Some(project_id) = self.plan_owner(session, cx) else {
            self.push_error("chat.messageBody.toast.noProject", None, window, cx);
            return;
        };
        let placeholder = locale::strings(cx).get("saveProjectPlanDialog.field.titlePlaceholder");
        let suggested = suggest_plan_title(&text);
        let title = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        title.update(cx, |title, cx| {
            title.set_value(suggested, window, cx);
            title.focus(window, cx);
        });
        self.message_actions.plan = Some(SavePlan {
            runtime: self.runtime.clone(),
            session: session.clone(),
            project_id,
            body: text,
            title,
            saving: false,
        });
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let Some(plan) = this.read(cx).message_actions.plan.as_ref() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let saving = plan.saving;
            let can_save = !saving && !plan.title.read(cx).value().trim().is_empty();
            let label = |key: &str| {
                div()
                    .text_size(rems(LABEL_SIZE))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(palette.foreground)
                    .child(strings.get(key))
            };
            let on_cancel = workspace.clone();
            let on_save = workspace.clone();
            let on_close = workspace.clone();
            dialog
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        // A save in flight still reports when it ends.
                        if this
                            .message_actions
                            .plan
                            .as_ref()
                            .is_some_and(|plan| !plan.saving)
                        {
                            this.message_actions.plan = None;
                        }
                        cx.notify();
                    });
                })
                .title(strings.get("saveProjectPlanDialog.title"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(
                            div()
                                .text_size(rems(LABEL_SIZE))
                                .text_color(palette.muted_foreground)
                                .child(strings.get("saveProjectPlanDialog.description")),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(label("saveProjectPlanDialog.field.title"))
                                .child(Input::new(&plan.title)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(label("saveProjectPlanDialog.field.contentPreview"))
                                .child(
                                    div()
                                        .id("save-plan-preview")
                                        .accessibility_id("save-plan-preview")
                                        .max_h(px(PREVIEW_MAX_HEIGHT))
                                        .overflow_y_scroll()
                                        .rounded(px(8.0))
                                        .bg(palette.subtle)
                                        .px(px(12.0))
                                        .py(px(8.0))
                                        .text_size(rems(META_SIZE))
                                        .text_color(palette.foreground)
                                        .child(SharedString::from(plan.body.clone())),
                                ),
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
                                "save-plan-cancel",
                                strings.get("saveProjectPlanDialog.actions.cancel"),
                                None,
                                palette.foreground,
                                &palette,
                            )
                            .when(saving, |this| this.opacity(0.5))
                            .on_click(move |_, window, cx| {
                                let _ = on_cancel
                                    .update(cx, |this, cx| this.cancel_save_plan(window, cx));
                            })
                            .test_support(),
                        )
                        .child(
                            dialog_button(
                                "save-plan-confirm",
                                strings.get(if saving {
                                    "saveProjectPlanDialog.actions.saving"
                                } else {
                                    "saveProjectPlanDialog.actions.save"
                                }),
                                Some(palette.primary),
                                palette.primary_foreground,
                                &palette,
                            )
                            .when(!can_save, |this| this.opacity(0.5))
                            .on_click(move |_, window, cx| {
                                let _ = on_save
                                    .update(cx, |this, cx| this.confirm_save_plan(window, cx));
                            })
                            .test_support(),
                        ),
                )
        });
        cx.notify();
    }

    /// The dialog's Cancel; a save in flight keeps the dialog.
    pub(crate) fn cancel_save_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .message_actions
            .plan
            .as_ref()
            .is_some_and(|plan| plan.saving)
        {
            return;
        }
        self.message_actions.plan = None;
        window.close_dialog(cx);
        cx.notify();
    }

    /// The dialog's Save: creates the plan under the trimmed title.
    pub(crate) fn confirm_save_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self.message_actions.plan.as_ref() else {
            return;
        };
        let title = plan.title.read(cx).value().trim().to_owned();
        if plan.saving || title.is_empty() {
            return;
        }
        let (runtime, session) = (plan.runtime.clone(), plan.session.clone());
        let (project_id, body) = (plan.project_id.clone(), plan.body.clone());
        let changed = project_id.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(
                    async move { client.create_plan(&project_id, &title, &body).await.is_ok() },
                )
            },
            move |this, saved, window, cx| {
                this.finish_save_plan(
                    &runtime,
                    &session,
                    &changed,
                    saved == Some(true),
                    window,
                    cx,
                )
            },
            window,
            cx,
        );
        if !started {
            self.push_error("chat.messageBody.toast.savePlanFailed", None, window, cx);
            return;
        }
        if let Some(plan) = self.message_actions.plan.as_mut() {
            plan.saving = true;
        }
        cx.notify();
    }

    /// Ends the save of the plan the dialog of `session` holds. Success
    /// closes the dialog; a failure keeps it for another try.
    pub(crate) fn finish_save_plan(
        &mut self,
        runtime: &RuntimeId,
        session: &SessionId,
        project_id: &str,
        saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.message_actions.plan.as_ref().is_some_and(|plan| {
            plan.saving && plan.runtime == *runtime && plan.session == *session
        });
        if open {
            if saved {
                self.message_actions.plan = None;
                window.close_dialog(cx);
            } else if let Some(plan) = self.message_actions.plan.as_mut() {
                plan.saving = false;
            }
        }
        // A switched runtime has nothing to show for it.
        if *runtime != self.runtime {
            return;
        }
        if saved {
            self.project_context_changed(project_id, cx);
            let text = locale::strings(cx).get("chat.messageBody.toast.planSaved");
            self.push_notice(Notification::success(text.clone()), text, window, cx);
        } else {
            self.push_error("chat.messageBody.toast.savePlanFailed", None, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AnyWindowHandle, TestAppContext};

    use super::*;
    use crate::workspace_tests::open;

    fn update<T>(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> T,
    ) -> T {
        cx.update_window(window, |_, window, cx| {
            let result = workspace.update(cx, |workspace, cx| f(workspace, window, cx));
            window.render_frame(cx);
            result
        })
        .expect("window is open")
    }

    /// Puts a plan of `session` in the dialog's state, as the menu item does
    /// once the session's owner is known.
    fn hold_plan(
        this: &mut Workspace,
        session: &SessionId,
        saving: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let title = cx.new(|cx| InputState::new(window, cx));
        title.update(cx, |title, cx| title.set_value("Ship it", window, cx));
        // The dialog the plan's outcome closes.
        window.open_dialog(cx, |dialog, _, _| dialog);
        this.message_actions.plan = Some(SavePlan {
            runtime: this.runtime.clone(),
            session: session.clone(),
            project_id: "project".to_owned(),
            body: "Ship it. Then rest.".to_owned(),
            title,
            saving,
        });
    }

    fn root_session(this: &Workspace) -> SessionId {
        this.sessions
            .active()
            .iter()
            .find(|record| record.parent_id.is_none() && record.directory.is_some())
            .expect("a root session")
            .id
            .clone()
    }

    #[test]
    fn the_title_is_the_first_sentence_without_its_marker() {
        for (text, title) in [
            ("# Plan the  rollout\n\nBody", "Plan the rollout"),
            ("\r\n\r\n- Ship it. Then rest.", "Ship it."),
            ("12. Fix the cache! Now", "Fix the cache!"),
            ("* Is v1.2 ready? Yes", "Is v1.2 ready?"),
            ("No mark here\nSecond line.", "No mark here"),
            ("-dash stays", "-dash stays"),
            ("1.5 is a number", "1.5 is a number"),
            ("日本語の計画。次へ", "日本語の計画。次へ"),
            ("Größe prüfen. Danach", "Größe prüfen."),
            ("   \n\n", "Plan"),
            ("", "Plan"),
            ("##", "Plan"),
        ] {
            assert_eq!(suggest_plan_title(text), title, "{text:?}");
        }
    }

    #[test]
    fn a_long_first_sentence_is_cut_at_the_limit() {
        let long = "é".repeat(200);
        let title = suggest_plan_title(&long);
        assert_eq!(title.chars().count(), TITLE_LIMIT);
        let spaced = format!("{} tail", "word ".repeat(40));
        let title = suggest_plan_title(&spaced);
        assert!(title.chars().count() <= TITLE_LIMIT);
        assert!(!title.ends_with(' '), "the cut is trimmed");
    }

    #[gpui_kit::test]
    fn empty_text_and_no_server_open_no_dialog(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |this, window, cx| {
            let session = root_session(this);
            assert!(
                !this.can_save_plan(&session, cx),
                "synthetic data has no server to save on"
            );
            this.open_save_plan_dialog(&session, "   ".to_owned(), window, cx);
            assert!(this.message_actions.plan.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_saved_plan_closes_the_dialog_and_says_so(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |this, window, cx| {
            let session = root_session(this);
            hold_plan(this, &session, true, window, cx);
            let runtime = this.runtime.clone();
            this.finish_save_plan(&runtime, &session, "project", true, window, cx);
            assert!(this.message_actions.plan.is_none());
            assert_eq!(
                this.session_management.notice,
                Some(locale::strings(cx).get("chat.messageBody.toast.planSaved"))
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_save_keeps_the_dialog_for_another_try(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |this, window, cx| {
            let session = root_session(this);
            hold_plan(this, &session, true, window, cx);
            let runtime = this.runtime.clone();
            this.finish_save_plan(&runtime, &session, "project", false, window, cx);
            let plan = this.message_actions.plan.as_ref().expect("still open");
            assert!(!plan.saving, "Save is offered again");
            assert_eq!(plan.title.read(cx).value().to_string(), "Ship it");
            assert_eq!(
                this.session_management.notice,
                Some(locale::strings(cx).get("chat.messageBody.toast.savePlanFailed"))
            );
        });
    }

    #[gpui_kit::test]
    fn saving_needs_a_title_and_cancel_waits_for_a_save(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |this, window, cx| {
            let session = root_session(this);
            hold_plan(this, &session, false, window, cx);
            let title = this
                .message_actions
                .plan
                .as_ref()
                .expect("held")
                .title
                .clone();
            title.update(cx, |title, cx| title.set_value("   ", window, cx));
            this.session_management.notice = None;
            this.confirm_save_plan(window, cx);
            assert!(
                this.session_management.notice.is_none()
                    && this
                        .message_actions
                        .plan
                        .as_ref()
                        .is_some_and(|plan| !plan.saving),
                "an empty title saves nothing"
            );
            title.update(cx, |title, cx| title.set_value("Ship it", window, cx));
            // Synthetic data has no server: the save cannot start.
            this.confirm_save_plan(window, cx);
            assert_eq!(
                this.session_management.notice,
                Some(locale::strings(cx).get("chat.messageBody.toast.savePlanFailed"))
            );
            if let Some(plan) = this.message_actions.plan.as_mut() {
                plan.saving = true;
            }
            this.cancel_save_plan(window, cx);
            assert!(
                this.message_actions.plan.is_some(),
                "a save in flight stays"
            );
            if let Some(plan) = this.message_actions.plan.as_mut() {
                plan.saving = false;
            }
            this.cancel_save_plan(window, cx);
            assert!(this.message_actions.plan.is_none());
        });
    }
}
