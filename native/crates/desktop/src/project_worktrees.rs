//! Project worktree management. Archive precedes removal so the server can
//! still resolve each session's location. A failed removal restores only the
//! sessions archived by that action. Every request retains its original host.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Role, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::ownership::{self, WorktreeInfo};
use openchamber_native_core::{path, session_moves};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::worktree::{Bootstrap, NewWorktree};

use crate::baseline::CssTextExt;
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::files_view::{Notice, PanelScope, status_line, text_button};
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;
use crate::worktree_removals::Outcome;

pub(crate) struct Manager {
    scope: PanelScope,
    rows: Vec<WorktreeInfo>,
    name: Entity<InputState>,
    confirm: Option<String>,
    delete_branch: bool,
    busy: bool,
    recovery: Option<u64>,
    notice: Option<Notice>,
    read: Option<Task<()>>,
}

impl Workspace {
    pub(crate) fn open_project_worktrees(
        &mut self,
        project: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_worktree_manager(project, None, window, cx);
    }

    pub(crate) fn open_worktree_removal(
        &mut self,
        directory: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.topology.worktrees.iter().find_map(|group| {
            group
                .worktrees
                .iter()
                .find(|worktree| path::is_within(directory, &worktree.path))
                .map(|worktree| (group.project_path.clone(), worktree.path.clone()))
        });
        if let Some((project, path)) = target {
            self.open_worktree_manager(&project, Some(path), window, cx);
        }
    }

    fn open_worktree_manager(
        &mut self,
        project: &str,
        confirm: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = &self.connection else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let scope = PanelScope {
            client,
            network: connection.network.handle(),
            runtime: self.runtime.clone(),
            directory: project.to_owned(),
            owner: Some(cx.entity().downgrade()),
        };
        self.show_worktree_manager(scope, confirm, window, cx);
    }

    pub(crate) fn show_worktree_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, runtime, network, recovery)) = self.worktree_removals.first_recovery() else {
            self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
            return;
        };
        if let Some(manager) = self.worktree_manager.as_ref().and_then(WeakEntity::upgrade)
            && manager.read(cx).scope.runtime == runtime
            && manager.read(cx).scope.directory == recovery.project
        {
            manager.update(cx, |manager, cx| {
                manager.busy = false;
                manager.recovery = Some(id);
                manager.notice = Some(removal_notice(&Outcome::Unresolved(recovery), cx));
                cx.notify();
            });
            return;
        }
        let scope = PanelScope {
            client: recovery.client,
            network,
            runtime,
            directory: recovery.project,
            owner: Some(cx.entity().downgrade()),
        };
        self.show_worktree_manager(scope, Some(recovery.target), window, cx);
    }

    fn show_worktree_manager(
        &mut self,
        scope: PanelScope,
        confirm: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recovery = self
            .worktree_removals
            .for_project(&scope.runtime, &scope.directory);
        let notice = recovery
            .and_then(|id| self.worktree_removals.outcome(id))
            .map(|outcome| removal_notice(&outcome, cx));
        let label = self.project_display_label(&scope.directory);
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(locale::strings(cx).get("session.newWorktree.branchName"))
        });
        let manager = cx.new(|cx| {
            let manager = Manager {
                scope,
                rows: Vec::new(),
                name,
                confirm,
                delete_branch: true,
                busy: false,
                recovery,
                notice,
                read: None,
            };
            let owner: WeakEntity<Manager> = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = owner.update(cx, |manager, cx| manager.refresh(cx));
            });
            manager
        });
        self.worktree_manager = Some(manager.downgrade());
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .w(px(560.0))
                .title(
                    locale::strings(cx)
                        .format("sessions.worktreesPage.title", &[("project", &label)]),
                )
                .child(manager.clone())
        });
    }
}

impl Manager {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let project = self.scope.directory.clone();
        let task = self.scope.run(async move { list(&client, &project).await });
        self.read = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.read = None;
                match result {
                    Ok(rows) => this.rows = rows,
                    Err(error) => {
                        this.notice = Some(Notice::error(
                            locale::strings(cx)
                                .get("sessions.sidebar.sessionDialogs.worktree.errorRemoveTitle"),
                            Some(&error.to_string()),
                        ))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.recovery.is_some() {
            return;
        }
        if self
            .scope
            .owner
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_none_or(|owner| owner.read(cx).close_in_progress())
        {
            return;
        }
        let name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let project = self.scope.directory.clone();
        self.busy = true;
        let owner = self.scope.owner.clone();
        let runtime = self.scope.runtime.clone();
        let task = self.scope.run(async move {
            let setup = client.worktree_setup(&project).await;
            let request = NewWorktree {
                name,
                start_ref: None,
                start_command: session_moves::start_command(
                    session_moves::worktree_setup_commands(&setup),
                    &project,
                ),
                generated: false,
            };
            let created = client.create_worktree(&project, &request).await?;
            for _ in 0..1200 {
                match client.worktree_bootstrap(&created.path).await? {
                    Bootstrap::Ready => return Ok(()),
                    Bootstrap::Failed(_) => {
                        return Err(TransportError::Malformed(
                            "worktree setup failed".to_owned(),
                        ));
                    }
                    Bootstrap::Pending { .. } => {
                        tokio::time::sleep(Duration::from_millis(250)).await
                    }
                }
            }
            Err(TransportError::Malformed(
                "worktree setup is still pending".to_owned(),
            ))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.notice = Some(match &result {
                    Some(Ok(())) => Notice::info(
                        locale::strings(cx).get("session.newWorktree.toast.worktreeCreated"),
                    ),
                    _ => Notice::error(
                        locale::strings(cx).get("session.newWorktree.error.createWorktreeFailed"),
                        None,
                    ),
                });
                this.refresh(cx);
            });
            refresh_owner(owner, runtime, None, cx);
        })
        .detach();
        cx.notify();
    }

    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.recovery.is_some() {
            return;
        }
        let Some(target) = self.confirm.clone() else {
            return;
        };
        // The root checkout and a stale arbitrary path never gain removal authority.
        if target == self.scope.directory || !self.rows.iter().any(|row| row.path == target) {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let Some(owner) = self.scope.owner.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let workspace = owner.read(cx);
        for session in workspace.sessions.active() {
            let Some(directory) = session.resolved_directory() else {
                continue;
            };
            if !path::is_within(&directory, &target) {
                continue;
            }
            if matches!(
                workspace.live_status.get(&session.id),
                Some(
                    openchamber_native_core::session::LiveStatus::Busy
                        | openchamber_native_core::session::LiveStatus::Retry
                )
            ) {
                self.notice = Some(Notice::error(
                    locale::strings(cx).get("sessions.sidebar.session.moveToWorktree.tooltipBusy"),
                    None,
                ));
                cx.notify();
                return;
            }
        }
        let project = self.scope.directory.clone();
        let delete_branch = self.delete_branch;
        let owner = self.scope.owner.clone();
        let runtime = self.scope.runtime.clone();
        self.busy = true;
        self.notice = None;
        let removed_path = target.clone();
        let operation = async move {
            // The local index can lag a different client's new session. Only a
            // complete host read establishes the sessions to archive.
            let sessions = client.all_sessions().await?;
            let statuses = client.session_statuses().await?;
            let mut batches: Vec<(String, Vec<SessionId>)> = Vec::new();
            for session in sessions
                .iter()
                .filter(|session| session.time.archived.is_none())
            {
                let Some(directory) = session
                    .resolved_directory()
                    .filter(|directory| path::is_within(directory, &target))
                else {
                    continue;
                };
                if matches!(
                    statuses.get(&session.id),
                    Some(
                        openchamber_native_core::session::LiveStatus::Busy
                            | openchamber_native_core::session::LiveStatus::Retry
                    )
                ) {
                    return Err(TransportError::Malformed(
                        "a worktree session is still running".to_owned(),
                    ));
                }
                if batches.iter().any(|(_, ids)| ids.contains(&session.id)) {
                    continue;
                }
                match batches.iter_mut().find(|(known, _)| known == &directory) {
                    Some((_, ids)) => ids.push(session.id.clone()),
                    None => batches.push((directory, vec![session.id.clone()])),
                }
            }
            Ok(remove_with_sessions(&client, &project, &target, delete_branch, batches).await)
        };
        let network = self.scope.network.clone();
        let accepted = owner
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|owner| {
                owner.update(cx, |workspace, _| {
                    if workspace.close_in_progress() {
                        return None;
                    }
                    workspace.worktree_removals.start(
                        runtime.clone(),
                        removed_path.clone(),
                        &network,
                        async move {
                            match operation.await {
                                Ok(outcome) => outcome,
                                Err(error) => Outcome::Failed(error.to_string()),
                            }
                        },
                    )
                })
            });
        let Some((operation_id, receipt)) = accepted else {
            self.busy = false;
            self.notice = Some(Notice::error(
                locale::strings(cx)
                    .get("sessions.sidebar.sessionDialogs.worktree.errorRemoveTitle"),
                None,
            ));
            cx.notify();
            return;
        };
        self.observe_removal(operation_id, receipt, removed_path, window, cx);
        cx.notify();
    }

    fn retry_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(id) = self.recovery else { return };
        let Some(owner) = self.scope.owner.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let Some(Outcome::Unresolved(recovery)) = owner.read(cx).worktree_removals.outcome(id)
        else {
            return;
        };
        let receipt = owner.update(cx, |workspace, _| workspace.worktree_removals.retry(id));
        let Some(receipt) = receipt else { return };
        self.busy = true;
        self.observe_removal(id, receipt, recovery.target, window, cx);
        cx.notify();
    }

    fn observe_removal(
        &mut self,
        operation_id: u64,
        receipt: tokio::sync::oneshot::Receiver<Outcome>,
        removed_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = self.scope.owner.clone();
        let runtime = self.scope.runtime.clone();
        // Custody belongs to the workspace even if this continuation disappears.
        cx.spawn_in(window, async move |this, cx| {
            let result = receipt.await.ok();
            let removed = matches!(result, Some(Outcome::Removed));
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.recovery =
                    matches!(result, Some(Outcome::Unresolved(_))).then_some(operation_id);
                if removed {
                    this.confirm = None;
                }
                this.notice = Some(match &result {
                    Some(outcome) => removal_notice(outcome, cx),
                    None => Notice::error(
                        locale::strings(cx)
                            .get("sessions.sidebar.sessionDialogs.worktree.errorRemoveTitle"),
                        None,
                    ),
                });
                this.refresh(cx);
            });
            if let Some(owner) = &owner {
                let _ = owner.update(cx, |workspace, _| {
                    workspace.worktree_removals.finish(operation_id)
                });
            }
            refresh_owner(owner, runtime, removed.then_some(removed_path), cx);
        })
        .detach();
    }
}

fn removal_notice(outcome: &Outcome, cx: &gpui_kit::App) -> Notice {
    match outcome.error() {
        None => Notice::info(
            locale::strings(cx).get("sessions.sidebar.sessionDialogs.worktree.removed"),
        ),
        Some(error) => Notice::error(
            locale::strings(cx).get("sessions.sidebar.sessionDialogs.worktree.errorRemoveTitle"),
            Some(error),
        ),
    }
}

fn refresh_owner(
    owner: Option<WeakEntity<Workspace>>,
    runtime: RuntimeId,
    removed: Option<String>,
    cx: &mut gpui_kit::AsyncWindowContext,
) {
    let Some(owner) = owner else { return };
    let _ = owner.update_in(cx, |workspace, window, cx| {
        if workspace.runtime != runtime {
            return;
        }
        if let Some(removed) = removed
            && workspace
                .selected_session()
                .and_then(|session| session.resolved_directory())
                .is_some_and(|directory| path::is_within(&directory, &removed))
        {
            workspace.show_new_session(window, cx);
        }
        workspace.load_topology(window, cx);
        workspace.load_sessions(window, cx);
    });
}

async fn list(client: &ServerClient, project: &str) -> Result<Vec<WorktreeInfo>, TransportError> {
    client
        .project_worktrees(&[project.to_owned()])
        .await?
        .into_iter()
        .next()
        .and_then(|read| read.worktrees)
        .map(|rows| ownership::linked_worktrees(project, rows))
        .ok_or_else(|| TransportError::Malformed("the worktree list is incomplete".to_owned()))
}

async fn remove_with_sessions(
    client: &ServerClient,
    project: &str,
    target: &str,
    delete_branch: bool,
    batches: Vec<(String, Vec<SessionId>)>,
) -> Outcome {
    let mut recovery = Recovery {
        client: client.clone(),
        project: project.to_owned(),
        target: target.to_owned(),
        ids: Vec::new(),
        error: String::new(),
    };
    for (directory, ids) in batches {
        for id in ids {
            // Only active sessions enter this operation. Retain the current ID
            // before I/O so a lost acknowledgement cannot omit it from rollback.
            // Singleton requests also place its archive in the server's per-session
            // chain before a rollback can enqueue the matching restore.
            recovery.ids.push(id.clone());
            let ids = [id];
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64;
            match client.archive_sessions(&directory, &ids, stamp).await {
                Ok(reply) => {
                    let accepted: Vec<SessionId> =
                        reply.archived.into_iter().map(|(id, _)| id).collect();
                    let complete = exact_receipt(&ids, &accepted, &reply.failed);
                    if !complete || !reply.failed.is_empty() {
                        recovery.error = "some sessions could not be archived".to_owned();
                        return recovery.retry().await;
                    }
                }
                Err(error) => {
                    recovery.error = error.to_string();
                    return recovery.retry().await;
                }
            }
        }
    }
    match client.remove_worktree(project, target, delete_branch).await {
        Ok(()) => Outcome::Removed,
        Err(error) => {
            recovery.error = error.to_string();
            recovery.retry().await
        }
    }
}

/// The exact attempted archive set stays attached to its original server until
/// a complete worktree read and, where needed, an exact restore receipt settle it.
#[derive(Clone)]
pub(crate) struct Recovery {
    pub(crate) client: ServerClient,
    pub(crate) project: String,
    pub(crate) target: String,
    pub(crate) ids: Vec<SessionId>,
    pub(crate) error: String,
}

impl Recovery {
    pub(crate) async fn retry(mut self) -> Outcome {
        // Never repeat DELETE. Its lost acknowledgement may already have removed
        // the checkout, in which case sessions must remain archived.
        let result: Result<bool, TransportError> = async {
            let rows = list(&self.client, &self.project).await?;
            if !rows.iter().any(|row| row.path == self.target) {
                return Ok(true);
            }
            restore(&self.client, &self.ids).await?;
            Ok(false)
        }
        .await;
        match result {
            Ok(true) => Outcome::Removed,
            Ok(false) => Outcome::Failed(self.error),
            Err(error) => {
                self.error = error.to_string();
                Outcome::Unresolved(Box::new(self))
            }
        }
    }
}

async fn restore(client: &ServerClient, ids: &[SessionId]) -> Result<(), TransportError> {
    if ids.is_empty() {
        return Ok(());
    }
    let reply = client.restore_sessions(ids).await?;
    if !exact_receipt(ids, &reply.restored, &reply.failed) || !reply.failed.is_empty() {
        return Err(TransportError::Malformed(
            "some sessions remain archived after removal failed".to_owned(),
        ));
    }
    Ok(())
}

fn exact_receipt(requested: &[SessionId], succeeded: &[SessionId], failed: &[SessionId]) -> bool {
    use std::collections::HashSet;
    let expected: HashSet<&SessionId> = requested.iter().collect();
    let answered: Vec<&SessionId> = succeeded.iter().chain(failed).collect();
    let unique: HashSet<&SessionId> = answered.iter().copied().collect();
    expected.len() == requested.len() && unique.len() == answered.len() && unique == expected
}

impl Render for Manager {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx);
        let direction = strings.direction();
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in &self.rows {
            let path = row.path.clone();
            rows.push(
                div()
                    .flex_row_from(direction)
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(ROW_SIZE, ROW_LINE, cx)
                            .text_color(palette.foreground)
                            .child(SharedString::from(
                                row.label.clone().unwrap_or_else(|| row.path.clone()),
                            )),
                    )
                    .child(text_button(
                        SharedString::from(format!("worktree-remove:{}", row.path)),
                        strings.get("mobile.projectEdit.deleteWorktreeConfirmButton"),
                        None,
                        &palette,
                        cx,
                        move |this: &mut Self, _, cx| {
                            if !this.busy {
                                this.confirm = Some(path.clone());
                                cx.notify();
                            }
                        },
                    ))
                    .into_any_element(),
            );
        }
        div()
            .id("worktree-manager")
            .role(Role::Group)
            .accessibility_id("worktree-manager")
            .aria_label(strings.get("sessions.sidebar.project.actions.manageWorktrees"))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .gap(px(8.0))
                    .child(
                        Input::new(&self.name)
                            .id("worktree-name")
                            .accessibility_id("worktree-name")
                            .aria_label(strings.get("session.newWorktree.branchName")),
                    )
                    .child(text_button(
                        "worktree-create",
                        strings.get("session.newWorktree.actions.createWorktree"),
                        None,
                        &palette,
                        cx,
                        |this: &mut Self, window, cx| this.create(window, cx),
                    )),
            )
            .child(
                div()
                    .id("worktree-list")
                    .max_h(px(300.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(rows),
            )
            .children(self.notice.as_ref().map(|notice| {
                status_line("worktree-notice", Some(notice), &palette, direction, cx)
            }))
            .when(self.recovery.is_some(), |body| {
                body.child(text_button(
                    "worktree-retry-recovery",
                    strings.get("diffView.actions.retry"),
                    None,
                    &palette,
                    cx,
                    |this: &mut Self, window, cx| this.retry_recovery(window, cx),
                ))
            })
            .when_some(self.confirm.as_ref(), |body, target| {
                body.child(
                    div()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.foreground)
                        .child(target.clone()),
                )
                .child(
                    div()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.status_warning)
                        .child(
                            strings
                                .get("sessions.sidebar.sessionDialogs.worktree.uncommittedWarning"),
                        ),
                )
                .child(text_button(
                    "worktree-delete-branch",
                    strings.get("sessions.sidebar.sessionDialogs.actions.deleteLocalBranch"),
                    Some(self.delete_branch),
                    &palette,
                    cx,
                    |this: &mut Self, _, cx| {
                        if !this.busy {
                            this.delete_branch = !this.delete_branch;
                            cx.notify();
                        }
                    },
                ))
                .child(
                    div()
                        .flex_row_from(direction)
                        .gap(px(8.0))
                        .child(text_button(
                            "worktree-remove-cancel",
                            strings.get("gitView.common.cancel"),
                            None,
                            &palette,
                            cx,
                            |this: &mut Self, _, cx| {
                                if !this.busy {
                                    this.confirm = None;
                                    cx.notify();
                                }
                            },
                        ))
                        .child(text_button(
                            "worktree-remove-confirm",
                            strings.get("mobile.projectEdit.deleteWorktreeConfirmButton"),
                            None,
                            &palette,
                            cx,
                            |this: &mut Self, window, cx| this.remove(window, cx),
                        )),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_requires_each_requested_session_once_and_no_unrequested_ids() {
        let a = SessionId::new("ses_a");
        let b = SessionId::new("ses_b");
        let requested = vec![a.clone(), b.clone()];
        assert!(exact_receipt(&requested, &requested, &[]));
        assert!(exact_receipt(
            &requested,
            std::slice::from_ref(&a),
            std::slice::from_ref(&b)
        ));
        assert!(!exact_receipt(&requested, std::slice::from_ref(&a), &[]));
        assert!(!exact_receipt(&requested, &[a.clone(), a], &[]));
        assert!(!exact_receipt(
            &requested,
            &[b, SessionId::new("ses_other")],
            &[]
        ));
        assert!(!exact_receipt(&requested, &requested, &requested));
    }
}
