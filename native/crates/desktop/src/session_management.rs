//! Session management from the sidebar row menu: rename, pin, work state,
//! archive, restore and delete.
//!
//! Follows `useSessionActions.ts`, `sessionSubtreeActions.ts`,
//! `session-actions.ts` and `session-pins.ts`. Every change except a pin
//! waits for the server and then goes through the same index path as the
//! live event that reports it, so a response that started earlier cannot
//! overwrite it. Each request captures its runtime and session; one failed
//! session never blocks another, and nothing is retried.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, SharedString, Task, Window};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::session::SessionPatch;
use openchamber_native_core::session_actions::{self, MenuContext, PendingPins, SessionAction};
use openchamber_native_transport::client::SettingsChanges;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::session_actions::{Archived, Deleted, Restored, WorkState};

use crate::connection::ConnectionState;
use crate::locale::{self, Strings};
use crate::send_attempt::SessionKey;
use crate::sidebar_item::SidebarItem;
use crate::workspace::Workspace;

/// Pending session changes and the tasks that carry them.
#[derive(Default)]
pub(crate) struct SessionManagement {
    pub(crate) pins: PendingPins,
    pin_queue: HashMap<SessionKey, VecDeque<bool>>,
    pin_in_flight: HashSet<SessionKey>,
    pub(crate) removing: HashSet<SessionKey>,
    pub(crate) renaming: HashSet<SessionKey>,
    pub(crate) changing_work: HashSet<SessionKey>,
    pub(crate) confirm: Option<RemovalConfirm>,
    /// The newest notification text, mirrored into a status node because
    /// the toolkit's notifications are not exposed to accessibility clients.
    pub(crate) notice: Option<SharedString>,
    pub(crate) deletion_dialog: DeletionDialogPreference,
    /// The row whose menu a press outside just closed, and where.
    pub(crate) menu_dismissed: Option<(SharedString, gpui_kit::Point<gpui_kit::Pixels>)>,
    tasks: HashMap<u64, Task<()>>,
    next_task: u64,
}

impl SessionManagement {
    /// Forgets client-side overlays for a runtime switch. Requests in
    /// flight finish against their captured runtime. Returns whether a
    /// confirmation was open, so its dialog closes too.
    pub(crate) fn switch_runtime(&mut self) -> bool {
        self.pins.clear();
        self.pin_queue.clear();
        self.confirm.take().is_some()
    }
}

/// The deletion dialog's Never ask choice while saves are in flight.
///
/// As the web client's settings store does, the checkbox follows the newest
/// click at once and saves coalesce to the newest choice, one at a time. The
/// web reports no failed settings save; a failure here returns the checkbox
/// to the stored choice.
#[derive(Debug, Default)]
pub(crate) struct DeletionDialogPreference {
    requested: Option<bool>,
    saving: bool,
}

impl DeletionDialogPreference {
    /// Whether the dialog asks, given the server's stored choice.
    pub(crate) fn shows(&self, stored: bool) -> bool {
        self.requested.unwrap_or(stored)
    }

    /// Records a click. Returns the value to save now, if any.
    pub(crate) fn toggle(&mut self, stored: bool) -> Option<bool> {
        self.requested = Some(!self.shows(stored));
        if self.saving {
            return None;
        }
        self.next(stored)
    }

    /// Records a finished save: the stored choice it left, or `None` when it
    /// failed. Returns the next value to save, if the newest click differs.
    pub(crate) fn finish(&mut self, stored: Option<bool>) -> Option<bool> {
        self.saving = false;
        match stored {
            Some(stored) => self.next(stored),
            None => {
                self.requested = None;
                None
            }
        }
    }

    fn next(&mut self, stored: bool) -> Option<bool> {
        match self.requested {
            Some(value) if value != stored => {
                self.saving = true;
                Some(value)
            }
            _ => {
                self.requested = None;
                None
            }
        }
    }
}

/// An archive or delete waiting for the user's confirmation, with the
/// sessions it reaches resolved when the dialog opened, so what runs matches
/// what the dialog said.
#[derive(Clone, Debug)]
pub(crate) struct RemovalConfirm {
    pub(crate) runtime: RuntimeId,
    pub(crate) title: String,
    /// The root first, then its descendants.
    pub(crate) targets: Vec<SessionId>,
    pub(crate) hard_delete: bool,
}

impl RemovalConfirm {
    pub(crate) fn description(&self, strings: &Strings) -> SharedString {
        let count = self.targets.len().saturating_sub(1);
        let count_text = count.to_string();
        let key = match (self.hard_delete, count) {
            (true, 0) => "sessions.sidebar.dialogs.deleteSession.single",
            (true, 1) => "sessions.sidebar.dialogs.deleteSession.withOneSubtask",
            (true, _) => "sessions.sidebar.dialogs.deleteSession.withManySubtasks",
            (false, 0) => "sessions.sidebar.dialogs.archiveSession.single",
            (false, 1) => "sessions.sidebar.dialogs.archiveSession.withOneSubtask",
            (false, _) => "sessions.sidebar.dialogs.archiveSession.withManySubtasks",
        };
        strings.format(
            key,
            &[("sessionTitle", &self.title), ("count", &count_text)],
        )
    }
}

/// One sentence a person can act on when a session action fails
/// (`describeSessionActionError`). The transport keeps no response body, so
/// an HTTP failure names its status.
pub(crate) fn describe_error(strings: &Strings, error: &TransportError) -> SharedString {
    match error {
        TransportError::Http { status } => strings.format(
            "sessions.sidebar.session.action.upstreamError",
            &[
                ("status", &status.to_string()),
                (
                    "message",
                    &strings.get("sessions.sidebar.session.action.noDetails"),
                ),
            ],
        ),
        other => other.to_string().into(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl Workspace {
    /// Whether a server can take a session change now.
    pub(crate) fn session_actions_available(&self) -> bool {
        self.connection.as_ref().is_some_and(|connection| {
            connection.state == ConnectionState::Connected && connection.client.is_some()
        })
    }

    /// What the menu of a session row offers.
    pub(crate) fn session_menu_context(&self, item: &SidebarItem) -> Option<MenuContext> {
        let SidebarItem::Session {
            session_id,
            owns_work,
            archived,
            ..
        } = item
        else {
            return None;
        };
        let session = self.sessions.get(session_id)?;
        let key = self.mutation_key(session_id);
        let settings = self.send_catalog.settings.as_ref();
        let in_chats = openchamber_native_core::chat_roots::is_chat_directory(
            self.topology.chat_roots.as_ref(),
            session.directory.as_deref(),
        );
        let management = &self.session_management;
        Some(MenuContext {
            pinned: management.pins.shows_pinned(session),
            can_track_work: session_actions::can_track_work(
                settings
                    .and_then(|settings| settings.work_enabled)
                    .unwrap_or(true),
                *owns_work,
                *archived,
                session,
                in_chats,
            ),
            in_work: session.overlay.in_work,
            archived: *archived || session.is_archived(),
            available: self.session_actions_available(),
            removing: management.removing.contains(&key),
            renaming: management.renaming.contains(&key),
            changing_work: management.changing_work.contains(&key),
        })
    }

    /// Runs one menu action for a session.
    pub(crate) fn run_session_action(
        &mut self,
        row_key: SharedString,
        session_id: SessionId,
        action: SessionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SessionAction::Rename => self.start_rename(row_key, session_id, window, cx),
            SessionAction::Pin => self.set_pinned(&session_id, true, window, cx),
            SessionAction::Unpin => self.set_pinned(&session_id, false, window, cx),
            SessionAction::TrackWork => {
                self.set_work_state(&session_id, WorkState::Open, window, cx)
            }
            SessionAction::MarkDone => {
                self.set_work_state(&session_id, WorkState::Done, window, cx)
            }
            SessionAction::Archive => self.request_removal(&session_id, false, window, cx),
            SessionAction::Delete => self.request_removal(&session_id, true, window, cx),
            SessionAction::Restore => {
                self.restore_sessions(vec![session_id], None, false, window, cx);
            }
        }
    }

    /// Runs `request` on the network and `finish` with its result on the
    /// window. The task belongs to the window; closing it cancels the
    /// request without stopping the server.
    fn spawn_session_task<T: Send + 'static>(
        &mut self,
        request: impl FnOnce(
            openchamber_native_transport::client::ServerClient,
        ) -> std::pin::Pin<Box<dyn Future<Output = T> + Send>>,
        finish: impl FnOnce(&mut Workspace, Option<T>, &mut Window, &mut Context<Workspace>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self.connection.as_ref() else {
            return false;
        };
        let Some(client) = connection.client.clone() else {
            return false;
        };
        let running = connection.network.run(request(client));
        let id = self.session_management.next_task;
        self.session_management.next_task += 1;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = running.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.session_management.tasks.remove(&id);
                finish(this, result, window, cx);
                cx.notify();
            });
        });
        self.session_management.tasks.insert(id, task);
        true
    }

    fn push_error(
        &mut self,
        title: &str,
        detail: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = locale::strings(cx).get(title);
        let notification = match detail {
            Some(detail) => Notification::error(detail).title(title.clone()),
            None => Notification::error(title.clone()),
        };
        self.push_notice(notification, title, window, cx);
    }

    /// Shows a notification and announces its text through the status node.
    fn push_notice(
        &mut self,
        notification: Notification,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.notice = Some(text);
        window.push_notification(notification, cx);
        cx.notify();
    }

    /// Commits an inline rename. The editor closes either way; a failure
    /// says what the server answered.
    pub(crate) fn commit_rename(
        &mut self,
        session_id: SessionId,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(&session_id);
        if self.session_management.renaming.contains(&key) {
            return;
        }
        let id = session_id.clone();
        let started = self.spawn_session_task(
            move |client| Box::pin(async move { client.rename_session(&id, &title).await }),
            move |this, result, window, cx| this.finish_rename(key, result, window, cx),
            window,
            cx,
        );
        if started {
            self.session_management
                .renaming
                .insert(self.mutation_key(&session_id));
        }
    }

    pub(crate) fn finish_rename(
        &mut self,
        key: SessionKey,
        result: Option<Result<openchamber_native_core::session::SessionRecord, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.renaming.remove(&key);
        match result {
            Some(Ok(record)) if key.runtime == self.runtime => {
                let event = ServerEvent::SessionPatched {
                    session_id: key.session,
                    patch: SessionPatch {
                        title: Some(record.title),
                        ..SessionPatch::default()
                    },
                };
                self.apply_server_events(vec![event], window, cx);
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                let detail = describe_error(locale::strings(cx), &error);
                let message = locale::strings(cx).format(
                    "sessions.sidebar.session.rename.error",
                    &[("detail", &detail)],
                );
                self.push_notice(Notification::error(message.clone()), message, window, cx);
            }
            None => {}
        }
    }

    /// Pins or unpins at once; the server write follows in click order. A
    /// row that already shows `pinned` writes nothing.
    pub(crate) fn set_pinned(
        &mut self,
        session_id: &SessionId,
        pinned: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session_actions_available() {
            return;
        }
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        if !self.session_management.pins.request(session, pinned) {
            return;
        }
        let key = self.mutation_key(session_id);
        self.session_management
            .pin_queue
            .entry(key.clone())
            .or_default()
            .push_back(pinned);
        self.rebuild_sidebar();
        self.pump_pin(key, window, cx);
        cx.notify();
    }

    fn pump_pin(&mut self, key: SessionKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.pin_in_flight.contains(&key) {
            return;
        }
        let Some(pinned) = self
            .session_management
            .pin_queue
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
        else {
            self.session_management.pin_queue.remove(&key);
            return;
        };
        let id = key.session.clone();
        let finished = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move { client.set_session_pinned(&id, pinned, now_ms()).await })
            },
            move |this, result, window, cx| this.finish_pin(finished, pinned, result, window, cx),
            window,
            cx,
        );
        if started {
            self.session_management.pin_in_flight.insert(key);
        } else {
            self.finish_pin(key, pinned, None, window, cx);
        }
    }

    pub(crate) fn finish_pin(
        &mut self,
        key: SessionKey,
        pinned: bool,
        result: Option<Result<openchamber_native_core::session::MetadataFacts, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.pin_in_flight.remove(&key);
        if key.runtime != self.runtime {
            return;
        }
        let mut events = Vec::new();
        match result {
            Some(Ok(metadata)) => events.push(ServerEvent::SessionPatched {
                session_id: key.session.clone(),
                patch: SessionPatch {
                    metadata: Some(metadata),
                    ..SessionPatch::default()
                },
            }),
            Some(Err(_)) | None => self.push_error(
                if pinned {
                    "sessions.sidebar.session.pin.pinFailed"
                } else {
                    "sessions.sidebar.session.pin.unpinFailed"
                },
                None,
                window,
                cx,
            ),
        }
        let released = self.session_management.pins.settle(&key.session);
        self.apply_server_events(events, window, cx);
        // The overlay left even when the index did not change.
        if released {
            self.rebuild_sidebar();
        }
        self.pump_pin(key, window, cx);
    }

    /// Tracks a work or marks it done; the row changes when the server
    /// confirms.
    pub(crate) fn set_work_state(
        &mut self,
        session_id: &SessionId,
        state: WorkState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(session_id);
        if self.session_management.changing_work.contains(&key) {
            return;
        }
        let id = session_id.clone();
        let finished = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move { client.set_session_work_state(&id, state, now_ms()).await })
            },
            move |this, result, window, cx| this.finish_work_state(finished, result, window, cx),
            window,
            cx,
        );
        if started {
            self.session_management.changing_work.insert(key);
            cx.notify();
        }
    }

    pub(crate) fn finish_work_state(
        &mut self,
        key: SessionKey,
        result: Option<Result<openchamber_native_core::session::MetadataFacts, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.changing_work.remove(&key);
        // A switched runtime has nothing to show for it.
        if key.runtime != self.runtime {
            return;
        }
        match result {
            Some(Ok(metadata)) => {
                let event = ServerEvent::SessionPatched {
                    session_id: key.session,
                    patch: SessionPatch {
                        metadata: Some(metadata),
                        ..SessionPatch::default()
                    },
                };
                self.apply_server_events(vec![event], window, cx);
            }
            Some(Err(_)) | None => self.push_error(
                "sessions.sidebar.session.work.updateFailed",
                None,
                window,
                cx,
            ),
        }
    }

    /// Archive or delete: asks first while the setting says so.
    pub(crate) fn request_removal(
        &mut self,
        session_id: &SessionId,
        hard_delete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session_actions_available() {
            return;
        }
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let targets = if hard_delete {
            session_actions::delete_targets(&self.sessions, session_id)
        } else {
            session_actions::archive_targets(&self.sessions, session_id)
        };
        let title = if session.title.is_empty() {
            locale::strings(cx)
                .get("sessions.sidebar.session.untitled")
                .to_string()
        } else {
            session.title.clone()
        };
        let confirm = RemovalConfirm {
            runtime: self.runtime.clone(),
            title,
            targets,
            hard_delete,
        };
        if self.shows_deletion_dialog() {
            self.open_removal_dialog(confirm, window, cx);
        } else {
            self.execute_removal(confirm, window, cx);
        }
    }

    /// The dialog's confirm button.
    pub(crate) fn confirm_removal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.session_management.confirm.take() else {
            return;
        };
        window.close_dialog(cx);
        if confirm.runtime == self.runtime {
            self.execute_removal(confirm, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn cancel_removal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.confirm.take().is_some() {
            window.close_dialog(cx);
        }
        cx.notify();
    }

    fn stored_deletion_dialog(&self) -> bool {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.show_deletion_dialog)
            .unwrap_or(true)
    }

    /// Whether archive and delete ask first: the newest Never ask click,
    /// else the server's stored choice.
    pub(crate) fn shows_deletion_dialog(&self) -> bool {
        self.session_management
            .deletion_dialog
            .shows(self.stored_deletion_dialog())
    }

    /// The dialog's Never ask toggle, saved as `showDeletionDialog`.
    pub(crate) fn toggle_deletion_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self.stored_deletion_dialog();
        if let Some(show) = self.session_management.deletion_dialog.toggle(stored) {
            self.save_deletion_dialog(show, window, cx);
        }
        cx.notify();
    }

    fn save_deletion_dialog(&mut self, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    client
                        .save_settings(&SettingsChanges {
                            show_deletion_dialog: Some(show),
                            ..SettingsChanges::default()
                        })
                        .await
                })
            },
            move |this, result, window, cx| {
                if this.runtime != runtime {
                    this.session_management.deletion_dialog = Default::default();
                    return;
                }
                // The merged document is authoritative for this runtime.
                let stored = match result {
                    Some(Ok(settings)) => {
                        this.send_catalog.settings = Some(settings);
                        Some(this.stored_deletion_dialog())
                    }
                    Some(Err(_)) | None => None,
                };
                if let Some(next) = this.session_management.deletion_dialog.finish(stored) {
                    this.save_deletion_dialog(next, window, cx);
                }
            },
            window,
            cx,
        );
        // Without a server nothing is stored; the checkbox shows the stored choice.
        if !started {
            self.session_management.deletion_dialog.finish(None);
        }
    }

    fn execute_removal(
        &mut self,
        confirm: RemovalConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<SessionKey> = confirm
            .targets
            .iter()
            .map(|id| self.mutation_key(id))
            .collect();
        if keys
            .first()
            .is_some_and(|key| self.session_management.removing.contains(key))
        {
            return;
        }
        let reopen = self
            .selected
            .clone()
            .filter(|selected| confirm.targets.contains(selected));
        let runtime = confirm.runtime.clone();
        let started = if confirm.hard_delete {
            let ids = confirm.targets.clone();
            self.spawn_session_task(
                move |client| {
                    Box::pin(async move {
                        // One by one: a failure leaves the others to run.
                        let mut outcomes = Vec::with_capacity(ids.len());
                        for id in ids {
                            let outcome = client.delete_session(&id).await;
                            outcomes.push((id, outcome));
                        }
                        outcomes
                    })
                },
                move |this, result, window, cx| {
                    this.finish_delete(runtime, confirm.targets, result, window, cx)
                },
                window,
                cx,
            )
        } else {
            // One request per directory, as `archiveSessions` batches. A
            // session without a known directory cannot be archived and
            // counts as failed.
            let mut batches: Vec<(String, Vec<SessionId>)> = Vec::new();
            for id in &confirm.targets {
                let Some(directory) = self
                    .sessions
                    .get(id)
                    .and_then(|session| session.resolved_directory())
                else {
                    continue;
                };
                match batches.iter_mut().find(|(known, _)| *known == directory) {
                    Some((_, ids)) => ids.push(id.clone()),
                    None => batches.push((directory, vec![id.clone()])),
                }
            }
            let targets = confirm.targets.clone();
            self.spawn_session_task(
                move |client| {
                    Box::pin(async move {
                        let archived_at = now_ms();
                        let mut outcomes = Vec::with_capacity(batches.len());
                        for (directory, ids) in batches {
                            let outcome =
                                client.archive_sessions(&directory, &ids, archived_at).await;
                            outcomes.push((ids, outcome));
                        }
                        outcomes
                    })
                },
                move |this, result, window, cx| {
                    this.finish_archive(runtime, targets, reopen, result, window, cx)
                },
                window,
                cx,
            )
        };
        if started {
            for key in keys {
                self.session_management.removing.insert(key);
            }
            cx.notify();
        }
    }

    pub(crate) fn finish_delete(
        &mut self,
        runtime: RuntimeId,
        targets: Vec<SessionId>,
        result: Option<Vec<(SessionId, Result<Deleted, TransportError>)>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in &targets {
            self.session_management.removing.remove(&SessionKey {
                runtime: runtime.clone(),
                session: id.clone(),
            });
        }
        let outcomes = result.unwrap_or_default();
        let stale = runtime != self.runtime;
        let mut deleted = Vec::new();
        let mut first_error = None;
        for (id, outcome) in outcomes {
            match outcome {
                Ok(_) => deleted.push(id),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if !stale {
            let events = deleted
                .iter()
                .map(|id| ServerEvent::SessionDeleted {
                    session_id: id.clone(),
                })
                .collect();
            self.apply_server_events(events, window, cx);
        }
        let failed = stale || deleted.len() < targets.len();
        let strings = locale::strings(cx).clone();
        if failed {
            let detail = first_error.map(|error| describe_error(&strings, &error));
            self.push_error("sessions.sidebar.session.delete.error", detail, window, cx);
        } else if targets.len() == 1 {
            let text = strings.get("sessions.sidebar.session.delete.success");
            self.push_notice(Notification::success(text.clone()), text, window, cx);
        } else {
            let key = if deleted.len() == 1 {
                "sessions.sidebar.bulkActions.deletedSingle"
            } else {
                "sessions.sidebar.bulkActions.deletedPlural"
            };
            let text = strings.format(key, &[("count", &deleted.len().to_string())]);
            self.push_notice(Notification::success(text.clone()), text, window, cx);
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn finish_archive(
        &mut self,
        runtime: RuntimeId,
        targets: Vec<SessionId>,
        reopen: Option<SessionId>,
        result: Option<Vec<(Vec<SessionId>, Result<Archived, TransportError>)>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in &targets {
            self.session_management.removing.remove(&SessionKey {
                runtime: runtime.clone(),
                session: id.clone(),
            });
        }
        let stale = runtime != self.runtime;
        let mut archived: Vec<(SessionId, i64)> = Vec::new();
        let mut first_error = None;
        for (ids, outcome) in result.unwrap_or_default() {
            match outcome {
                // Only stamps for sessions this batch asked about count.
                Ok(answer) => archived.extend(
                    answer
                        .archived
                        .into_iter()
                        .filter(|(id, _)| ids.contains(id)),
                ),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if stale {
            archived.clear();
        }
        let events = archived
            .iter()
            .map(|(id, stamp)| ServerEvent::SessionPatched {
                session_id: id.clone(),
                patch: SessionPatch {
                    archived: Some(Some(*stamp)),
                    ..SessionPatch::default()
                },
            })
            .collect();
        self.apply_server_events(events, window, cx);
        let strings = locale::strings(cx).clone();
        let archived_ids: Vec<SessionId> = archived.into_iter().map(|(id, _)| id).collect();
        let failed = targets.len() - archived_ids.len();
        let detail = first_error.map(|error| describe_error(&strings, &error));
        if targets.len() == 1 {
            if failed == 0 {
                self.push_archive_success(
                    strings.get("sessions.sidebar.session.archive.success"),
                    archived_ids,
                    reopen,
                    window,
                    cx,
                );
            } else {
                self.push_error("sessions.sidebar.session.archive.error", detail, window, cx);
            }
            return;
        }
        if !archived_ids.is_empty() {
            let key = if archived_ids.len() == 1 {
                "sessions.sidebar.bulkActions.archivedSingle"
            } else {
                "sessions.sidebar.bulkActions.archivedPlural"
            };
            let message = strings.format(key, &[("count", &archived_ids.len().to_string())]);
            self.push_archive_success(message, archived_ids, reopen, window, cx);
        }
        if failed > 0 {
            let key = if failed == 1 {
                "sessions.sidebar.bulkActions.failedArchiveSingle"
            } else {
                "sessions.sidebar.bulkActions.failedArchivePlural"
            };
            let title = strings.format(key, &[("count", &failed.to_string())]);
            let notification = match detail {
                Some(detail) => Notification::error(detail).title(title.clone()),
                None => Notification::error(title.clone()),
            };
            self.push_notice(notification, title, window, cx);
        }
    }

    /// The archive toast offers Undo for exactly the sessions it moved.
    fn push_archive_success(
        &mut self,
        message: SharedString,
        archived: Vec<SessionId>,
        reopen: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let undo = locale::strings(cx).get("sessions.sidebar.session.archive.undo");
        let workspace = cx.entity().downgrade();
        let runtime = self.runtime.clone();
        let notification = Notification::success(message.clone())
            .action(move |_, _, cx| {
                let workspace = workspace.clone();
                let archived = archived.clone();
                let reopen = reopen.clone();
                let runtime = runtime.clone();
                let notification = cx.entity().downgrade();
                Button::new("session-archive-undo")
                    .accessibility_id("session-archive-undo")
                    .label(undo.clone())
                    .on_click(move |_, window, cx| {
                        let dismiss = workspace
                            .update(cx, |this, cx| {
                                this.undo_archive(
                                    &runtime,
                                    archived.clone(),
                                    reopen.clone(),
                                    window,
                                    cx,
                                )
                            })
                            .unwrap_or(true);
                        if dismiss {
                            let _ = notification
                                .update(cx, |notification, cx| notification.dismiss(window, cx));
                        }
                    })
            })
            .autohide(true);
        self.push_notice(notification, message, window, cx);
    }

    /// The archive toast's Undo. Returns whether the toast should close: a
    /// restore that cannot start keeps Undo offered, while a toast from
    /// another runtime can never act again (`expectedRuntimeKey`).
    pub(crate) fn undo_archive(
        &mut self,
        runtime: &RuntimeId,
        archived: Vec<SessionId>,
        reopen: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if *runtime != self.runtime {
            self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
            return true;
        }
        self.restore_sessions(archived, reopen, true, window, cx)
    }

    /// Restores sessions. A root brings back its archived subsessions; an
    /// undo restores exactly the sessions its archive moved and reopens the
    /// session the archive closed while nothing else was opened since.
    /// Returns whether the restore started; when it cannot, it says so.
    pub(crate) fn restore_sessions(
        &mut self,
        roots: Vec<SessionId>,
        reopen: Option<SessionId>,
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.session_actions_available() {
            self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
            return false;
        }
        let mut order: Vec<SessionId> = Vec::new();
        for root in &roots {
            if order.contains(root) {
                continue;
            }
            if !undo {
                let restorable = self.sessions.get(root).is_some_and(|session| {
                    session_actions::restorable_on_its_own(&self.sessions, session)
                });
                if !restorable {
                    continue;
                }
            }
            order.push(root.clone());
            if !undo {
                order.extend(session_actions::archived_subsessions(&self.sessions, root));
            }
        }
        if order.is_empty() {
            self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
            return false;
        }
        let keys: Vec<SessionKey> = order.iter().map(|id| self.mutation_key(id)).collect();
        if keys
            .iter()
            .any(|key| self.session_management.removing.contains(key))
        {
            self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
            return false;
        }
        let runtime = self.runtime.clone();
        let ids = order.clone();
        let roots_requested = roots.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    // One by one, parents first: a root that fails keeps its
                    // subsessions archived with it.
                    let mut outcomes: Vec<(SessionId, Result<Restored, TransportError>)> =
                        Vec::new();
                    for id in ids {
                        let outcome = client.restore_sessions(std::slice::from_ref(&id)).await;
                        let failed_root = !undo
                            && roots_requested.contains(&id)
                            && !matches!(&outcome, Ok(restored) if restored.restored.contains(&id));
                        outcomes.push((id, outcome));
                        if failed_root {
                            break;
                        }
                    }
                    outcomes
                })
            },
            move |this, result, window, cx| {
                this.finish_restore(runtime, order, roots, reopen, undo, result, window, cx)
            },
            window,
            cx,
        );
        if !started {
            self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
            return false;
        }
        for key in keys {
            self.session_management.removing.insert(key);
        }
        cx.notify();
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_restore(
        &mut self,
        runtime: RuntimeId,
        requested: Vec<SessionId>,
        roots: Vec<SessionId>,
        reopen: Option<SessionId>,
        undo: bool,
        result: Option<Vec<(SessionId, Result<Restored, TransportError>)>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in &requested {
            self.session_management.removing.remove(&SessionKey {
                runtime: runtime.clone(),
                session: id.clone(),
            });
        }
        let stale = runtime != self.runtime;
        let restored: Vec<SessionId> = result
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, outcome)| match outcome {
                Ok(answer) if answer.restored.contains(&id) => Some(id),
                _ => None,
            })
            .collect();
        if !stale {
            let events = restored
                .iter()
                .map(|id| ServerEvent::SessionPatched {
                    session_id: id.clone(),
                    patch: SessionPatch {
                        archived: Some(None),
                        ..SessionPatch::default()
                    },
                })
                .collect();
            self.apply_server_events(events, window, cx);
        }
        let all_roots = !stale && roots.iter().all(|root| restored.contains(root));
        let all = !stale && requested.iter().all(|id| restored.contains(id));
        if undo {
            // An undo reports only what failed, as `undoArchive` does.
            if !all {
                self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
            }
        } else if all_roots {
            let text = locale::strings(cx).get("sessions.sidebar.session.restore.success");
            self.push_notice(Notification::success(text.clone()), text, window, cx);
        } else {
            self.push_error("sessions.sidebar.session.restore.error", None, window, cx);
        }
        if let Some(reopen) = reopen
            && !stale
            && self.selected.is_none()
            && restored.contains(&reopen)
        {
            self.select_session(reopen, window, cx);
        }
    }
}
