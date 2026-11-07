//! The Move session dialog: picks the project folder a conversation moves
//! to (`MoveSessionDialog.tsx`). The conversation and its subsessions move;
//! nothing on disk does, which the description says before anything runs.
//! Choose folder registers another folder as a project first and then moves
//! there.

use gpui_kit::component::WindowExt;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::path;

use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `typography-ui-label` and `typography-micro`.
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const MICRO_SIZE: f32 = 0.75;
const MICRO_LINE: f32 = 1.333_333;

/// `formatPathForDisplay`: the home folder as `~`.
pub(crate) fn display_path(path: &str, home: Option<&str>) -> String {
    match home.and_then(path::normalize) {
        Some(home) if path == home => "~".to_owned(),
        Some(home) => path
            .strip_prefix(&format!("{home}/"))
            .map_or_else(|| path.to_owned(), |rest| format!("~/{rest}")),
        None => path.to_owned(),
    }
}

impl Workspace {
    /// The root a move takes: the session climbs to the top ancestor in the
    /// same folder, so a subsession never leaves its parent behind.
    fn move_root(&self, session: &SessionId) -> Option<SessionId> {
        let mut root = self.sessions.get(session)?;
        while let Some(parent) = root
            .parent_id
            .as_ref()
            .and_then(|parent| self.sessions.get(parent))
            .filter(|parent| parent.directory == root.directory)
        {
            root = parent;
        }
        Some(root.id.clone())
    }

    /// Move session from the row menu.
    pub(crate) fn open_move_session_dialog(
        &mut self,
        session: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self
            .sessions
            .get(session)
            .and_then(|record| record.resolved_directory())
        else {
            return;
        };
        self.session_management.move_dialog = Some((session.clone(), source));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let state = this.read(cx);
            let Some((session, source)) = state.session_management.move_dialog.clone() else {
                return dialog;
            };
            let home = state
                .send_catalog
                .settings
                .as_ref()
                .and_then(|settings| settings.home_directory.clone());
            let projects: Vec<(String, SharedString)> = state
                .topology
                .projects
                .iter()
                .map(|project| {
                    (
                        project.path.clone(),
                        state.project_display_label(&project.path),
                    )
                })
                .collect();
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let current = path::normalize(&source);
            let hover = palette.hover;
            let row = |id: String, icon: &'static str, label: SharedString| {
                div()
                    .id(SharedString::from(id.clone()))
                    .role(Role::Button)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .min_w_0()
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(glyph(icon, 16.0, palette.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(LABEL_SIZE, LABEL_LINE, cx)
                            .text_color(palette.foreground)
                            .child(label),
                    )
            };
            let rows = projects.into_iter().map(|(project, label)| {
                let is_current = current.as_deref() == Some(project.as_str());
                let target = project.clone();
                let workspace = workspace.clone();
                let session = session.clone();
                row(format!("move-session-project:{project}"), "folder", label)
                    .when(!is_current, |this| {
                        this.cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .on_click(move |_, window, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.session_management.move_dialog = None;
                                    window.close_dialog(cx);
                                    if let Some(root) = this.move_root(&session) {
                                        this.move_session_to_project(&root, &target, window, cx);
                                    }
                                });
                            })
                    })
                    .when(is_current, |this| {
                        this.opacity(0.5)
                            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    })
                    .child(
                        div()
                            .max_w(px(160.0))
                            .min_w_0()
                            .truncate()
                            .css_text(MICRO_SIZE, MICRO_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(display_path(&project, home.as_deref())),
                    )
                    .when(is_current, |this| {
                        this.child(
                            div()
                                .id("move-session-current")
                                .aria_label(strings.get("sessions.moveSession.current"))
                                .child(glyph("check", 14.0, palette.primary)),
                        )
                    })
                    .test_support()
                    .into_any_element()
            });
            let choose = {
                let workspace = workspace.clone();
                let session = session.clone();
                row(
                    "move-session-choose-folder".to_owned(),
                    "folder-add",
                    strings.get("sessions.moveSession.chooseFolder"),
                )
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(move |_, window, cx| {
                    let _ = workspace.update(cx, |this, cx| {
                        this.session_management.move_dialog = None;
                        window.close_dialog(cx);
                        this.open_add_project(Some(session.clone()), window, cx);
                    });
                })
                .test_support()
                .into_any_element()
            };
            let on_close = workspace.clone();
            dialog
                .w(px(448.0))
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.session_management.move_dialog = None;
                        cx.notify();
                    });
                })
                .title(strings.get("sessions.moveSession.title"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(
                            div()
                                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(strings.get("sessions.moveSession.description")),
                        )
                        .child(
                            div()
                                .id("move-session-projects")
                                .max_h(px(320.0))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .children(rows)
                                .child(choose),
                        ),
                )
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_home_show_a_tilde() {
        assert_eq!(display_path("/home/u/app", Some("/home/u")), "~/app");
        assert_eq!(display_path("/home/u", Some("/home/u/")), "~");
        assert_eq!(
            display_path("/home/user2/app", Some("/home/u")),
            "/home/user2/app"
        );
        assert_eq!(display_path("/srv", None), "/srv");
    }
}
