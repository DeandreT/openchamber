//! The sidebar's Select sessions mode (`useSidebarBulkActions.ts`,
//! `BulkActionBar.tsx`): rows check instead of opening, and a bar over the
//! list archives, restores or deletes what is checked.
//!
//! A selection lives in one project, as `core::session_multi_select` keeps
//! it: a row under a project header carries that project as its scope, and
//! the zones, Chats and the list by time carry none, so their rows select
//! across projects. The actions run through the same archive, delete and restore paths as
//! a single row's: each session is asked on its own, one that fails neither
//! stops nor undoes the others, and the toast counts what worked and what
//! did not. Sessions that leave the list leave the selection.
//!
//! Moving the selection to a folder and the mode's keyboard shortcuts
//! (Escape, select all, delete) are not ported.

use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::session_actions;
use openchamber_native_core::session_index::SessionIndex;
use openchamber_native_core::session_multi_select::{self, SelectableRow};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{ButtonTone, CONTROL_RADIUS, IconButton, glyph};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::session_management::RemovalConfirm;
use crate::sidebar_item::SidebarItem;
use crate::theme::Palette;
use crate::workspace::Workspace;

const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;

impl Workspace {
    /// The Display mode menu's Select sessions and Exit selection.
    pub(crate) fn toggle_selection_mode(&mut self, cx: &mut Context<Self>) {
        self.session_management.selection.toggle_mode();
        cx.notify();
    }

    /// The bar's close button: the rows open sessions again.
    pub(crate) fn exit_selection_mode(&mut self, cx: &mut Context<Self>) {
        self.session_management.selection.disable();
        cx.notify();
    }

    /// Every session row as the selection reads it, in display order. One
    /// pass: a row's scope is the project header it sits under.
    fn selectable_rows(&self) -> Vec<SelectableRow> {
        let mut scope: Option<String> = None;
        let mut rows = Vec::new();
        for item in &self.sidebar_items {
            match item {
                SidebarItem::Project { project_id, .. } => scope = Some(project_id.to_string()),
                // Zone lists carry no selection scope.
                SidebarItem::Zone { .. } => scope = None,
                SidebarItem::Session {
                    key, session_id, ..
                } => rows.push(SelectableRow {
                    id: session_id.clone(),
                    row_key: key.to_string(),
                    scope: scope.clone(),
                }),
                _ => {}
            }
        }
        rows
    }

    /// A click on a session row while the mode is on. `range` (a shift
    /// click) checks every row from the last checked one to this one.
    pub(crate) fn toggle_row_selected(
        &mut self,
        row_key: &str,
        range: bool,
        cx: &mut Context<Self>,
    ) {
        let rows = self.selectable_rows();
        let sessions = &self.sessions;
        let selection = &mut self.session_management.selection;
        if range {
            // Subsessions are resolved for the rows inside the range only.
            selection.select_range(&rows, row_key, |row| row_descendants(sessions, &row.id));
        } else if let Some(row) = rows.iter().find(|row| row.row_key == row_key) {
            selection.toggle(row, &row_descendants(sessions, &row.id));
        }
        cx.notify();
    }

    /// The checked sessions the list still holds, oldest check first.
    fn selected_sessions(&self) -> Vec<SessionId> {
        self.session_management
            .selection
            .selected()
            .iter()
            .filter(|id| self.sessions.get(id).is_some())
            .cloned()
            .collect()
    }

    /// The bar's Archive and Delete. Archive leaves already archived
    /// sessions as they are; Delete removes each checked session's whole
    /// subtree. Asks first while the deletion dialog is on.
    pub(crate) fn request_bulk_removal(
        &mut self,
        hard_delete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session_actions_available() {
            return;
        }
        let selected = self.selected_sessions();
        let targets: Vec<SessionId> = if hard_delete {
            session_multi_select::bulk_targets(
                &self.sessions,
                &selected,
                session_actions::delete_targets,
            )
        } else {
            session_multi_select::bulk_targets(
                &self.sessions,
                &selected,
                session_actions::archive_targets,
            )
            .into_iter()
            .filter(|id| {
                self.sessions
                    .get(id)
                    .is_some_and(|session| !session.is_archived())
            })
            .collect()
        };
        if targets.is_empty() {
            return;
        }
        let confirm = RemovalConfirm {
            runtime: self.runtime.clone(),
            title: String::new(),
            targets,
            hard_delete,
            bulk: true,
        };
        if self.shows_deletion_dialog() {
            self.open_removal_dialog(confirm, window, cx);
        } else {
            self.execute_removal(confirm, window, cx);
        }
    }

    /// The bar's Restore, offered while every checked session is archived.
    pub(crate) fn restore_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .session_management
            .selection
            .all_archived(&self.sessions)
        {
            return;
        }
        // Every checked session is asked for, as `handleBulkRestore` does: a
        // subsession under a checked parent comes back with that parent, and
        // one whose parent failed counts as failed with it.
        let selected = self.selected_sessions();
        if selected.is_empty() {
            return;
        }
        self.restore_sessions(selected, None, false, window, cx);
    }

    /// The bar under the toolbar while the mode is on: how many sessions are
    /// checked, then Restore or Archive, Delete and the way out.
    pub(crate) fn render_bulk_bar(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let selection = &self.session_management.selection;
        if !selection.is_enabled() {
            return None;
        }
        let strings = locale::strings(cx).clone();
        let count = self.selected_sessions().len();
        let all_archived = selection.all_archived(&self.sessions);
        let count_label = strings.format(
            "sessions.sidebar.bulkActions.selectedCount",
            &[("count", &count.to_string())],
        );
        let button = |id: &'static str, icon: &'static str, key: &str| {
            IconButton::new(id, icon, strings.get(key))
                .size(28.0, 16.0)
                .tone(ButtonTone::QuietFilled)
        };
        let primary = if all_archived {
            button(
                "bulk-restore",
                "inbox-unarchive",
                "sessions.sidebar.bulkActions.restore",
            )
            .render(palette, cx, |this, window, cx| {
                this.restore_selection(window, cx)
            })
        } else {
            button(
                "bulk-archive",
                "archive",
                "sessions.sidebar.bulkActions.archive",
            )
            .render(palette, cx, |this, window, cx| {
                this.request_bulk_removal(false, window, cx)
            })
        };
        let error = palette.error;
        let delete = on_activate(
            div()
                .id("bulk-delete")
                .role(Role::Button)
                .accessibility_id("bulk-delete")
                .aria_label(strings.get("sessions.sidebar.bulkActions.delete"))
                .size(px(28.0))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(move |style| style.bg(error.opacity(0.1)))
                .child(glyph("delete-bin", 16.0, error)),
            cx,
            |this, window, cx| this.request_bulk_removal(true, window, cx),
        )
        .test_support();
        let exit = button(
            "bulk-exit",
            "close",
            "sessions.sidebar.header.actions.exitSelection",
        )
        .render(palette, cx, |this, _, cx| this.exit_selection_mode(cx));
        // Nothing checked: the actions stay in place, dimmed, and do nothing.
        let actions = div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(2.0))
            .opacity(if count == 0 { 0.4 } else { 1.0 })
            .child(primary)
            .child(delete);
        Some(
            div()
                .id("bulk-action-bar")
                .role(Role::Group)
                .accessibility_id("bulk-action-bar")
                .aria_label(count_label.clone())
                .flex_shrink_0()
                .h(px(40.0))
                .px(px(10.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id("bulk-selected-count")
                        .role(Role::Status)
                        .accessibility_id("bulk-selected-count")
                        .aria_label(count_label.clone())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(count_label),
                )
                .child(actions)
                .child(exit)
                .test_support()
                .into_any_element(),
        )
    }
}

/// The sessions a row carries along: what its own archive would reach, or
/// for an archived row the archived subsessions a restore brings back.
fn row_descendants(index: &SessionIndex, id: &SessionId) -> Vec<SessionId> {
    if index.get(id).is_some_and(SessionRecord::is_archived) {
        session_actions::archived_subsessions(index, id)
    } else {
        session_actions::archive_targets(index, id)
            .into_iter()
            .skip(1)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, SharedString, TestAppContext};
    use openchamber_native_core::ids::SessionId;
    use openchamber_native_transport::error::TransportError;
    use openchamber_native_transport::session_actions::{Archived, Deleted};

    use crate::sidebar_item::SidebarItem;
    use crate::workspace::Workspace;
    use crate::workspace_tests::open;

    fn name(id: impl Into<String>) -> ElementId {
        ElementId::Name(SharedString::from(id.into()))
    }

    /// Top-level session rows as (row key, session ID), in display order.
    fn rows(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> Vec<(SharedString, SessionId)> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .sidebar_items
                .iter()
                .filter_map(|item| match item {
                    SidebarItem::Session {
                        key,
                        session_id,
                        depth: 0,
                        has_children: false,
                        ..
                    } => Some((key.clone(), session_id.clone())),
                    _ => None,
                })
                .collect()
        })
    }

    fn update(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>),
    ) {
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| f(workspace, window, cx));
            window.render_frame(cx);
        })
        .expect("window is open");
    }

    fn checked(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<SessionId> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .session_management
                .selection
                .selected()
                .to_vec()
        })
    }

    fn notice(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .session_management
                .notice
                .as_ref()
                .map(ToString::to_string)
        })
    }

    #[gpui_kit::test]
    fn the_mode_checks_rows_instead_of_opening_them(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let (key, session) = rows(&workspace, cx)[0].clone();
        let before = cx.read(|cx| workspace.read(cx).selected.clone());
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find(name("bulk-action-bar")).is_none(),
                "the bar shows only in the mode"
            );
        })
        .expect("window is open");
        update(window, &workspace, cx, |state, _, cx| {
            state.toggle_selection_mode(cx)
        });
        cx.update_window(window, |_, window, cx| {
            assert!(window.try_find(name("bulk-action-bar")).is_some());
            window.click(name(format!("{key}:target")), cx);
            window.render_frame(cx);
        })
        .expect("window is open");
        assert_eq!(checked(&workspace, cx), std::slice::from_ref(&session));
        assert_eq!(
            cx.read(|cx| workspace.read(cx).selected.clone()),
            before,
            "a click in the mode opens nothing"
        );
        cx.update_window(window, |_, window, cx| {
            window.click(name("bulk-exit"), cx);
            window.render_frame(cx);
            assert!(window.try_find(name("bulk-action-bar")).is_none());
        })
        .expect("window is open");
        assert!(
            checked(&workspace, cx).is_empty(),
            "leaving forgets the checks"
        );
    }

    /// Session rows as (row key, session, the header they sit under), where
    /// the header is a project's ID or `None` for a zone.
    fn rows_with_headers(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> Vec<(SharedString, SessionId, Option<String>)> {
        cx.read(|cx| {
            let mut header = None;
            let mut rows = Vec::new();
            for item in &workspace.read(cx).sidebar_items {
                match item {
                    SidebarItem::Project { project_id, .. } => {
                        header = Some(project_id.to_string());
                    }
                    SidebarItem::Zone { .. } => header = None,
                    SidebarItem::Session {
                        key, session_id, ..
                    } => rows.push((key.clone(), session_id.clone(), header.clone())),
                    _ => {}
                }
            }
            rows
        })
    }

    /// The project header with the most session rows under it.
    fn busiest_project(rows: &[(SharedString, SessionId, Option<String>)]) -> String {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for header in rows.iter().filter_map(|(_, _, header)| header.as_deref()) {
            match counts.iter_mut().find(|(known, _)| *known == header) {
                Some((_, count)) => *count += 1,
                None => counts.push((header, 1)),
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(header, _)| header.to_owned())
            .expect("the fixture lists sessions under a project")
    }

    fn scope(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .session_management
                .selection
                .scope()
                .map(str::to_owned)
        })
    }

    #[gpui_kit::test]
    fn a_range_checks_the_rows_between_two_clicks_of_one_project(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |state, _, cx| {
            state.toggle_selection_mode(cx)
        });
        let all = rows_with_headers(&workspace, cx);
        let project = busiest_project(&all);
        let scoped: Vec<&(SharedString, SessionId, Option<String>)> = all
            .iter()
            .filter(|(_, _, header)| header.as_deref() == Some(project.as_str()))
            .collect();
        assert!(scoped.len() >= 3, "the fixture lists a project's sessions");
        let (first, last) = (scoped[0].0.clone(), scoped[2].0.clone());
        update(window, &workspace, cx, |state, _, cx| {
            state.toggle_row_selected(&first, false, cx);
            state.toggle_row_selected(&last, true, cx);
        });
        let selected = checked(&workspace, cx);
        for (_, session, _) in &scoped[..3] {
            assert!(selected.contains(session), "{session} is inside the range");
        }
        assert_eq!(scope(&workspace, cx), Some(project));
    }

    #[gpui_kit::test]
    fn zone_rows_carry_no_scope_and_project_rows_carry_their_header(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |state, _, cx| {
            state.toggle_selection_mode(cx)
        });
        let all = rows_with_headers(&workspace, cx);
        let listed = cx.read(|cx| workspace.read(cx).selectable_rows());
        assert_eq!(listed.len(), all.len());
        for (row, (key, session, header)) in listed.iter().zip(&all) {
            assert_eq!(row.row_key, key.to_string());
            assert_eq!(&row.id, session);
            assert_eq!(&row.scope, header, "{key} takes its header's scope");
        }
        // A zone row, whatever project its session belongs to, sets no scope.
        if let Some((key, session, _)) = all.iter().find(|(_, _, header)| header.is_none()) {
            let key = key.clone();
            update(window, &workspace, cx, |state, _, cx| {
                state.toggle_row_selected(&key, false, cx)
            });
            assert!(checked(&workspace, cx).contains(session));
            assert_eq!(scope(&workspace, cx), None);
        }
    }

    #[gpui_kit::test]
    fn rows_grouped_by_time_select_across_projects(cx: &mut TestAppContext) {
        use openchamber_native_core::sidebar_rows::SidebarViewMode;
        use openchamber_native_transport::client::ServerSettings;

        let (window, workspace) = open(cx);
        update(window, &workspace, cx, |state, _, cx| {
            state.send_catalog.settings = Some(ServerSettings {
                sidebar_view: Some(SidebarViewMode::WorksByTime),
                ..ServerSettings::default()
            });
            state.rebuild_sidebar();
            state.toggle_selection_mode(cx);
        });
        let all = rows_with_headers(&workspace, cx);
        assert!(all.len() >= 2, "the list by time has rows");
        assert!(
            all.iter().all(|(_, _, header)| header.is_none()),
            "the list by time has no project headers"
        );
        let (first, last) = (all[0].0.clone(), all[all.len() - 1].0.clone());
        update(window, &workspace, cx, |state, _, cx| {
            state.toggle_row_selected(&first, false, cx);
            state.toggle_row_selected(&last, true, cx);
        });
        let selected = checked(&workspace, cx);
        for (_, session, _) in &all {
            assert!(
                selected.contains(session),
                "{session} is in a range that no project boundary cuts"
            );
        }
        assert_eq!(scope(&workspace, cx), None);
    }

    #[gpui_kit::test]
    fn a_session_removed_by_the_server_leaves_the_selection(cx: &mut TestAppContext) {
        use openchamber_native_transport::event::ServerEvent;

        let (window, workspace) = open(cx);
        let all = rows(&workspace, cx);
        let (kept, removed) = (all[0].clone(), all[1].clone());
        update(window, &workspace, cx, |state, window, cx| {
            state.toggle_selection_mode(cx);
            state.toggle_row_selected(&removed.0, false, cx);
            // Another client deletes it; the event arrives over the stream.
            state.apply_server_events(
                vec![ServerEvent::SessionDeleted {
                    session_id: removed.1.clone(),
                }],
                window,
                cx,
            );
        });
        assert!(
            checked(&workspace, cx).is_empty(),
            "the deleted session is checked nowhere"
        );
        assert_eq!(scope(&workspace, cx), None);
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(state.sessions.get(&kept.1).is_some());
            assert!(
                !state
                    .session_management
                    .selection
                    .all_archived(&state.sessions),
                "an empty selection restores nothing"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_session_neither_blocks_nor_undoes_the_others(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let all = rows(&workspace, cx);
        let (first, second, third) = (all[0].1.clone(), all[1].1.clone(), all[2].1.clone());
        let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
        update(window, &workspace, cx, |state, window, cx| {
            state.toggle_selection_mode(cx);
            // The session that fails is checked last, so it is checked
            // whichever projects the three rows belong to.
            state.toggle_row_selected(&all[0].0, false, cx);
            state.toggle_row_selected(&all[2].0, false, cx);
            state.toggle_row_selected(&all[1].0, false, cx);
            // The server answered each session on its own: the middle one failed.
            state.finish_delete(
                runtime.clone(),
                vec![first.clone(), second.clone(), third.clone()],
                vec![
                    (first.clone(), vec![first.clone()]),
                    (second.clone(), vec![second.clone()]),
                    (third.clone(), vec![third.clone()]),
                ],
                Some(vec![
                    (first.clone(), Ok(Deleted::Removed)),
                    (second.clone(), Err(TransportError::TimedOut)),
                    (third.clone(), Ok(Deleted::Removed)),
                ]),
                window,
                cx,
            );
        });
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(state.sessions.get(&first).is_none());
            assert!(
                state.sessions.get(&third).is_none(),
                "the failure stopped nothing"
            );
            assert!(
                state.sessions.get(&second).is_some(),
                "the failed one stays"
            );
        });
        let selected = checked(&workspace, cx);
        assert!(
            !selected.contains(&first) && !selected.contains(&third),
            "what was deleted leaves the selection"
        );
        assert!(
            selected.contains(&second),
            "the failed session stays checked"
        );
        let strings = cx.read(|cx| crate::locale::strings(cx).clone());
        assert_eq!(
            notice(&workspace, cx),
            Some(
                strings
                    .format(
                        "sessions.sidebar.bulkActions.failedDeleteSingle",
                        &[("count", "1")]
                    )
                    .to_string()
            ),
            "the failure is reported after the deletions"
        );
    }

    #[gpui_kit::test]
    fn a_partly_failed_archive_keeps_the_archived_sessions_archived(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let all = rows(&workspace, cx);
        let (first, second) = (all[0].1.clone(), all[1].1.clone());
        let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
        update(window, &workspace, cx, |state, window, cx| {
            state.toggle_selection_mode(cx);
            state.toggle_row_selected(&all[0].0, false, cx);
            state.toggle_row_selected(&all[1].0, false, cx);
            state.finish_archive(
                runtime.clone(),
                vec![first.clone(), second.clone()],
                None,
                Some(vec![
                    (
                        vec![first.clone()],
                        Ok(Archived {
                            archived: vec![(first.clone(), 7)],
                            failed: Vec::new(),
                        }),
                    ),
                    (vec![second.clone()], Err(TransportError::TimedOut)),
                ]),
                window,
                cx,
            );
        });
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(
                state
                    .sessions
                    .get(&first)
                    .is_some_and(|session| session.is_archived())
            );
            assert!(
                state
                    .sessions
                    .get(&second)
                    .is_some_and(|session| !session.is_archived())
            );
        });
        let selected = checked(&workspace, cx);
        assert!(!selected.contains(&first));
        assert!(selected.contains(&second));
    }

    fn pending_confirm(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> Option<(Vec<SessionId>, bool)> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .session_management
                .confirm
                .as_ref()
                .map(|confirm| (confirm.targets.clone(), confirm.hard_delete))
        })
    }

    #[gpui_kit::test]
    fn a_bulk_confirmation_is_shown_again_when_its_sessions_changed(cx: &mut TestAppContext) {
        use crate::session_management::RemovalConfirm;
        use openchamber_native_core::session::SessionPatch;
        use openchamber_native_transport::event::ServerEvent;

        let (window, workspace) = open(cx);
        let all = rows(&workspace, cx);
        let (first, second) = (all[0].1.clone(), all[1].1.clone());
        let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
        let gone = SessionId::from("deleted-elsewhere");
        let ask = |targets: Vec<SessionId>, hard_delete: bool| RemovalConfirm {
            runtime: runtime.clone(),
            title: String::new(),
            targets,
            hard_delete,
            bulk: true,
        };
        // One of the three is gone when Delete is pressed: nothing runs yet,
        // and the dialog now counts two.
        let confirm = ask(vec![first.clone(), gone.clone(), second.clone()], true);
        update(window, &workspace, cx, |state, window, cx| {
            state.session_management.confirm = Some(confirm);
            state.confirm_removal(window, cx);
        });
        assert_eq!(
            pending_confirm(&workspace, cx),
            Some((vec![first.clone(), second.clone()], true))
        );
        cx.read(|cx| {
            let state = workspace.read(cx);
            assert!(state.sessions.get(&first).is_some(), "nothing was deleted");
            assert!(state.session_management.removing.is_empty());
        });
        // The same set, pressed again, is ready and leaves the dialog.
        update(window, &workspace, cx, |state, window, cx| {
            state.confirm_removal(window, cx)
        });
        assert_eq!(pending_confirm(&workspace, cx), None);
        // Every session is gone: the dialog closes with nothing to do.
        let confirm = ask(vec![gone.clone()], false);
        update(window, &workspace, cx, |state, window, cx| {
            state.session_management.confirm = Some(confirm);
            state.confirm_removal(window, cx);
        });
        assert_eq!(pending_confirm(&workspace, cx), None);
        // An archive whose only session was archived meanwhile asks again,
        // as a delete.
        let confirm = ask(vec![first.clone()], false);
        update(window, &workspace, cx, |state, window, cx| {
            state.apply_server_events(
                vec![ServerEvent::SessionPatched {
                    session_id: first.clone(),
                    patch: SessionPatch {
                        archived: Some(Some(9)),
                        ..SessionPatch::default()
                    },
                }],
                window,
                cx,
            );
            state.session_management.confirm = Some(confirm);
            state.confirm_removal(window, cx);
        });
        assert_eq!(
            pending_confirm(&workspace, cx),
            Some((vec![first.clone()], true))
        );
        // A mixed archive leaves the archived session as it is.
        let confirm = ask(vec![first.clone(), second.clone()], false);
        update(window, &workspace, cx, |state, window, cx| {
            state.session_management.confirm = Some(confirm);
            state.confirm_removal(window, cx);
        });
        assert_eq!(
            pending_confirm(&workspace, cx),
            Some((vec![second.clone()], false))
        );
    }

    #[gpui_kit::test]
    fn a_bulk_action_skips_sessions_a_request_is_already_removing(cx: &mut TestAppContext) {
        let (_, workspace) = open(cx);
        let all = rows(&workspace, cx);
        let (first, second) = (all[0].1.clone(), all[1].1.clone());
        cx.update(|cx| {
            workspace.update(cx, |state, _| {
                let key = state.mutation_key(&first);
                state.session_management.removing.insert(key);
                assert_eq!(
                    state.not_being_removed(vec![first.clone(), second.clone()]),
                    std::slice::from_ref(&second),
                    "only the session no request holds is asked for"
                );
                assert!(
                    state.not_being_removed(vec![first.clone()]).is_empty(),
                    "nothing remains, so nothing is sent"
                );
                let key = state.mutation_key(&second);
                assert!(
                    !state.session_management.removing.contains(&key),
                    "checking a target marks nothing"
                );
            });
        });
    }

    #[gpui_kit::test]
    fn a_bulk_restore_counts_every_checked_session(cx: &mut TestAppContext) {
        use openchamber_native_transport::session_actions::Restored;

        let (window, workspace) = open(cx);
        let all = rows_with_headers(&workspace, cx);
        let project = busiest_project(&all);
        let scoped: Vec<(SharedString, SessionId)> = all
            .iter()
            .filter(|(_, _, header)| header.as_deref() == Some(project.as_str()))
            .map(|(key, session, _)| (key.clone(), session.clone()))
            .collect();
        assert!(scoped.len() >= 3, "the fixture lists a project's sessions");
        let (back, failed_root, skipped) = (
            scoped[0].1.clone(),
            scoped[1].1.clone(),
            scoped[2].1.clone(),
        );
        let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
        update(window, &workspace, cx, |state, window, cx| {
            state.toggle_selection_mode(cx);
            for (key, _) in &scoped[..3] {
                state.toggle_row_selected(key, false, cx);
            }
            // The selection named three sessions. One came back, one root
            // failed, and the third was never asked because it sits under
            // the root that failed.
            let every = vec![back.clone(), failed_root.clone(), skipped.clone()];
            state.finish_restore(
                runtime.clone(),
                every.clone(),
                every,
                None,
                false,
                Some(vec![
                    (
                        back.clone(),
                        Ok(Restored {
                            restored: vec![back.clone()],
                            failed: Vec::new(),
                        }),
                    ),
                    (failed_root.clone(), Err(TransportError::TimedOut)),
                ]),
                window,
                cx,
            );
        });
        let selected = checked(&workspace, cx);
        assert!(!selected.contains(&back), "what came back is unchecked");
        assert!(selected.contains(&failed_root));
        assert!(
            selected.contains(&skipped),
            "a session skipped under a failed root stays checked"
        );
        let strings = cx.read(|cx| crate::locale::strings(cx).clone());
        assert_eq!(
            notice(&workspace, cx),
            Some(
                strings
                    .format(
                        "sessions.sidebar.bulkActions.failedRestorePlural",
                        &[("count", "2")]
                    )
                    .to_string()
            ),
            "the skipped session counts as failed with its root"
        );
    }

    #[gpui_kit::test]
    fn a_bulk_confirmation_counts_sessions(cx: &mut TestAppContext) {
        use crate::session_management::RemovalConfirm;
        let (_, workspace) = open(cx);
        let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
        let strings = cx.read(|cx| crate::locale::strings(cx).clone());
        let confirm = |targets: usize, hard_delete: bool| RemovalConfirm {
            runtime: runtime.clone(),
            title: String::new(),
            targets: (0..targets)
                .map(|index| SessionId::new(format!("s{index}")))
                .collect(),
            hard_delete,
            bulk: true,
        };
        assert_eq!(
            confirm(3, false).title_key(),
            "sessions.sidebar.dialogs.archiveSessions.title"
        );
        assert_eq!(
            confirm(1, true).title_key(),
            "sessions.sidebar.dialogs.deleteSession.title"
        );
        assert_eq!(
            confirm(3, true).description(&strings),
            strings.format(
                "sessions.sidebar.dialogs.deleteSessions.pluralDescription",
                &[("count", "3")]
            )
        );
        assert_eq!(
            confirm(1, false).description(&strings),
            strings.format(
                "sessions.sidebar.dialogs.archiveSessions.singleDescription",
                &[("count", "1")]
            )
        );
    }
}
