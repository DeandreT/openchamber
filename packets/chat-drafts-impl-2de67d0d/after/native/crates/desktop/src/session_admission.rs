//! Session-folder admission tied to live moves and deliberate navigation.
//!
//! The raw index is never filtered here. Each pending destination belongs to the
//! original runtime and connection generation. A trust answer grants only its
//! displayed fingerprint; project and worktree ownership remains server-owned.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{DialogLayerHandle, Disableable, WindowExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    Context, InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, Subscription,
    Task, Window, div, px,
};
use openchamber_native_core::chat_roots;
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::ownership::ConfiguredProject;
use openchamber_native_core::path;
use openchamber_native_core::permission_mode;
use openchamber_native_core::session_index::{SessionChange, SessionMove};
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::project_admission::{
    ProjectAdmission, TrustAnswer, TrustAssessment, TrustItem, TrustKind,
};

use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::{Topology, Workspace};

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const ADMISSION_WAIT: Duration = Duration::from_secs(10);

/// Admission holds a slot only through I/O, never while awaiting an answer.
#[derive(Clone, Debug)]
struct NetworkSlots(Arc<tokio::sync::Semaphore>);

impl Default for NetworkSlots {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(2)))
    }
}

#[derive(Debug)]
enum Phase {
    Checking,
    Queued,
    Prompt(TrustAssessment),
    Saving(TrustAssessment),
    Registering,
    Waiting,
}

struct Attempt {
    token: u64,
    runtime: RuntimeId,
    generation: u64,
    sessions: HashSet<SessionId>,
    deliberate: bool,
    automatic: bool,
    navigation: Option<(SessionId, u64)>,
    retries: usize,
    phase: Phase,
    task: Option<Task<()>>,
}

struct DeferredNavigation {
    runtime: RuntimeId,
    session: SessionId,
    revision: u64,
    _observer: Subscription,
}

impl fmt::Debug for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attempt")
            .field("token", &self.token)
            .field("runtime", &self.runtime)
            .field("generation", &self.generation)
            .field("sessions", &self.sessions)
            .field("phase", &self.phase)
            .field("retries", &self.retries)
            .finish_non_exhaustive()
    }
}

/// Runtime-scoped admission, navigation continuations and topology read ordering.
#[derive(Default)]
pub(crate) struct SessionAdmission {
    slots: NetworkSlots,
    attempts: HashMap<String, Attempt>,
    next_token: u64,
    prompts: VecDeque<String>,
    prompt: Option<String>,
    prompt_dialog: Option<DialogLayerHandle>,
    declined: HashSet<(String, Option<String>)>,
    parked: Option<(SessionId, u64)>,
    deferred_navigation: Option<DeferredNavigation>,
    lookups: HashMap<SessionId, Task<()>>,
    pub(crate) topology_refresh: Option<Task<()>>,
    pub(crate) topology_ticket: u64,
    pub(crate) topology_revision: u64,
    pub(crate) confirmed_projects: Vec<(u64, ConfiguredProject)>,
    pub(crate) roots_ready: bool,
}

impl fmt::Debug for SessionAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionAdmission")
            .field("attempts", &self.attempts.len())
            .field("prompt_dialog", &self.prompt_dialog)
            .field("parked", &self.parked)
            .finish_non_exhaustive()
    }
}

impl SessionAdmission {
    /// Overlays only appends confirmed after this registry read began.
    pub(crate) fn reconcile_projects(&self, baseline: u64, projects: &mut Vec<ConfiguredProject>) {
        for (_, project) in self
            .confirmed_projects
            .iter()
            .filter(|(revision, _)| *revision > baseline)
        {
            if !projects.iter().any(|held| held.id == project.id) {
                projects.push(project.clone());
            }
        }
    }
}

/// The same admission predicate used by sidebar membership, including managed Chats.
pub(crate) fn directory_admitted(topology: &Topology, directory: &str) -> bool {
    let Some(directory) = path::normalize(directory) else {
        return false;
    };
    if chat_roots::is_chat_directory(topology.chat_roots.as_ref(), Some(&directory))
        || permission_mode::is_space_directory(&directory)
    {
        return true;
    }
    let contains = |root: &str| {
        path::normalize(root).is_some_and(|root| {
            !directory.split('/').any(|part| matches!(part, "." | ".."))
                && (directory == root
                    || directory
                        .strip_prefix(&root)
                        .is_some_and(|rest| root.ends_with('/') || rest.starts_with('/')))
        })
    };
    topology
        .projects
        .iter()
        .any(|project| contains(&project.path))
        || topology.worktrees.iter().any(|entry| {
            topology.projects.iter().any(|project| {
                path::normalize(&project.path) == path::normalize(&entry.project_path)
            }) && entry
                .worktrees
                .iter()
                .any(|worktree| contains(&worktree.path))
        })
}

impl Workspace {
    pub(crate) fn session_admitted(&self, id: &SessionId) -> bool {
        self.sessions
            .get(id)
            .and_then(|session| session.resolved_directory())
            .is_some_and(|directory| directory_admitted(&self.topology, &directory))
    }

    /// Invalidates every continuation before replacing a host or connection owner.
    pub(crate) fn reset_session_admission(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = self.session_admission.prompt_dialog.take();
        let next_token = self.session_admission.next_token.wrapping_add(1);
        let topology_ticket = self.session_admission.topology_ticket.wrapping_add(1);
        self.session_admission = SessionAdmission::default();
        self.session_admission.next_token = next_token;
        self.session_admission.topology_ticket = topology_ticket;
        if let Some(dialog) = dialog {
            window.close_dialog_by_handle(&dialog, cx);
        }
    }

    /// Returns true only when selection can proceed without creating a location.
    pub(crate) fn gate_session_open(
        &mut self,
        id: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.session_admission.parked = None;
        if self.session_admitted(id) {
            return true;
        }
        if let Some(directory) = self
            .sessions
            .get(id)
            .and_then(|session| session.resolved_directory())
        {
            self.start_session_admission(
                id.clone(),
                directory,
                Some(self.navigation_revision),
                window,
                cx,
            );
            return false;
        }
        let Some(connection) = self.connection.as_ref() else {
            return false;
        };
        let Some(client) = connection.client.clone() else {
            return false;
        };
        let generation = connection.generation;
        let runtime = self.runtime.clone();
        let revision = self.navigation_revision;
        let id = id.clone();
        let read_id = id.clone();
        let task = connection
            .network
            .run(async move { client.session(&read_id).await });
        let key = id.clone();
        let read = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.runtime != runtime
                    || this.navigation_revision != revision
                    || this
                        .connection
                        .as_ref()
                        .is_none_or(|connection| connection.generation != generation)
                {
                    return;
                }
                this.session_admission.lookups.remove(&id);
                match result {
                    Some(Ok(session)) if session.id == id => {
                        let update = this.sessions.apply([SessionChange::Created(session)]);
                        this.after_index_update(update, window, cx);
                        if this.session_admitted(&id) {
                            this.continue_admitted_navigation(id, revision, window, cx);
                        } else if let Some(directory) = this
                            .sessions
                            .get(&id)
                            .and_then(|session| session.resolved_directory())
                        {
                            this.start_session_admission(id, directory, Some(revision), window, cx);
                        }
                    }
                    _ => this.admission_failed_notice(window, cx),
                }
            });
        });
        self.session_admission.lookups.insert(key, read);
        false
    }

    /// Only live patches leaving an admitted source can admit in the background.
    pub(crate) fn observe_session_moves(
        &mut self,
        moves: &[SessionMove],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for moved in moves {
            if moved
                .from
                .as_deref()
                .is_some_and(|from| directory_admitted(&self.topology, from))
                && !directory_admitted(&self.topology, &moved.directory)
            {
                self.start_session_admission(
                    moved.session_id.clone(),
                    moved.directory.clone(),
                    None,
                    window,
                    cx,
                );
            }
        }
    }

    fn start_session_admission(
        &mut self,
        id: SessionId,
        directory: String,
        navigation: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = path::normalize(&directory) else {
            return;
        };
        if permission_mode::is_space_directory(&directory) {
            return;
        }
        if let Some(attempt) = self.session_admission.attempts.get_mut(&directory) {
            attempt.sessions.insert(id.clone());
            attempt.automatic |= navigation.is_none();
            if let Some(revision) = navigation {
                attempt.deliberate = true;
                attempt.navigation = Some((id, revision));
            }
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.client.is_some())
        else {
            return;
        };
        self.session_admission.next_token = self.session_admission.next_token.wrapping_add(1);
        let token = self.session_admission.next_token;
        self.session_admission.attempts.insert(
            directory.clone(),
            Attempt {
                token,
                runtime: self.runtime.clone(),
                generation: connection.generation,
                sessions: HashSet::from([id.clone()]),
                deliberate: navigation.is_some(),
                automatic: navigation.is_none(),
                navigation: navigation.map(|revision| (id, revision)),
                retries: 0,
                phase: Phase::Checking,
                task: None,
            },
        );
        self.check_session_admission(directory, token, window, cx);
    }

    fn current_admission(&self, directory: &str, token: u64) -> Option<SessionId> {
        let attempt = self.session_admission.attempts.get(directory)?;
        if attempt.token != token
            || attempt.runtime != self.runtime
            || self
                .connection
                .as_ref()
                .is_none_or(|connection| connection.generation != attempt.generation)
        {
            return None;
        }
        attempt
            .sessions
            .iter()
            .find(|id| {
                self.sessions
                    .get(id)
                    .and_then(|session| session.resolved_directory())
                    .as_deref()
                    == Some(directory)
            })
            .cloned()
    }

    fn check_session_admission(
        &mut self,
        directory: String,
        token: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_admission(&directory, token).is_none() {
            if self
                .session_admission
                .attempts
                .get(&directory)
                .is_some_and(|attempt| attempt.token == token)
            {
                self.finish_session_admission(&directory, false, window, cx);
            }
            return;
        }
        if directory_admitted(&self.topology, &directory) {
            self.finish_session_admission(&directory, true, window, cx);
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let read_directory = directory.clone();
        let need_roots = !self.session_admission.roots_ready;
        let slots = self.session_admission.slots.0.clone();
        let task = connection.network.run(async move {
            let _permit = slots
                .acquire_owned()
                .await
                .map_err(|_| TransportError::Cancelled)?;
            // Root discovery starts no OpenCode location. A failed root read
            // cannot turn a managed Chat into an ordinary imported project.
            let roots = if need_roots {
                Some(client.chat_roots().await?)
            } else {
                None
            };
            if roots.as_ref().is_some_and(|roots| {
                chat_roots::is_chat_directory(roots.as_ref(), Some(&read_directory))
            }) {
                return Ok((roots, None));
            }
            client
                .folder_trust(&read_directory)
                .await
                .map(|assessment| (roots, Some(assessment)))
        });
        let key = directory.clone();
        let read = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.current_admission(&directory, token).is_none() {
                    return;
                }
                let assessment = match result {
                    Some(Ok((roots, assessment))) => {
                        if let Some(roots) = roots {
                            this.topology.chat_roots = roots;
                            this.session_admission.roots_ready = true;
                            this.chat_roots_read(true, window, cx);
                        }
                        assessment
                    }
                    _ => {
                        this.retry_session_admission(directory, token, window, cx);
                        return;
                    }
                };
                match assessment {
                    None => {
                        this.rebuild_sidebar();
                        this.finish_session_admission(&directory, true, window, cx);
                    }
                    Some(assessment) if assessment.trusted => {
                        this.register_session_directory(directory, token, window, cx)
                    }
                    Some(assessment) => {
                        let deliberate = this.session_admission.attempts[&directory].deliberate;
                        if !deliberate
                            && this.session_admission.declined.contains(&(
                                assessment.directory.clone(),
                                assessment.fingerprint.clone(),
                            ))
                        {
                            this.finish_session_admission(&directory, false, window, cx);
                        } else {
                            this.session_admission
                                .attempts
                                .get_mut(&directory)
                                .expect("current attempt")
                                .phase = Phase::Queued;
                            this.session_admission.prompts.push_back(directory);
                            this.show_next_trust_prompt(window, cx);
                        }
                    }
                }
            });
        });
        if let Some(attempt) = self.session_admission.attempts.get_mut(&key) {
            attempt.phase = Phase::Checking;
            attempt.task = Some(read);
        }
    }

    fn register_session_directory(
        &mut self,
        directory: String,
        token: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.current_admission(&directory, token) else {
            return;
        };
        if directory_admitted(&self.topology, &directory) {
            self.finish_session_admission(&directory, true, window, cx);
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let request_directory = directory.clone();
        let slots = self.session_admission.slots.0.clone();
        let task = connection.network.run(async move {
            let _permit = slots
                .acquire_owned()
                .await
                .map_err(|_| TransportError::Cancelled)?;
            client
                .admit_session_directory(&id, &request_directory)
                .await
        });
        let key = directory.clone();
        let read = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.current_admission(&directory, token).is_none() {
                    return;
                }
                match result {
                    Some(Ok(ProjectAdmission::Confirmed(project))) => {
                        if !this
                            .topology
                            .projects
                            .iter()
                            .any(|held| held.id == project.id)
                        {
                            this.session_admission.topology_revision =
                                this.session_admission.topology_revision.wrapping_add(1);
                            this.session_admission
                                .confirmed_projects
                                .push((this.session_admission.topology_revision, project.clone()));
                            this.topology.projects.push(project.clone());
                            if let Some(settings) = &mut this.send_catalog.settings {
                                settings.projects.push(project);
                            }
                        }
                        this.rebuild_sidebar();
                        if directory_admitted(&this.topology, &directory) {
                            this.finish_session_admission(&directory, true, window, cx);
                        } else {
                            // The returned project can own an unlisted linked worktree.
                            // Only a successful worktree listing grants that membership.
                            this.load_topology(window, cx);
                            this.wait_for_session_admission(directory, token, window, cx);
                        }
                        this.reconcile_session_admission(window, cx);
                    }
                    Some(Ok(ProjectAdmission::SessionMoved)) => {
                        this.finish_session_admission(&directory, false, window, cx)
                    }
                    Some(Ok(ProjectAdmission::TrustRequired)) | None | Some(Err(_)) => {
                        this.retry_session_admission(directory, token, window, cx)
                    }
                }
            });
        });
        if let Some(attempt) = self.session_admission.attempts.get_mut(&key) {
            attempt.phase = Phase::Registering;
            attempt.task = Some(read);
        }
    }

    fn retry_session_admission(
        &mut self,
        directory: String,
        token: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attempt) = self
            .session_admission
            .attempts
            .get_mut(&directory)
            .filter(|attempt| attempt.token == token)
        else {
            return;
        };
        // Explicit opens report their failure. Automatic retries belong to live moves.
        let delay = retry_delay(attempt);
        let Some(delay) = delay else {
            if attempt.deliberate {
                self.admission_failed_notice(window, cx);
            }
            self.finish_session_admission(&directory, false, window, cx);
            return;
        };
        let key = directory.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.check_session_admission(directory, token, window, cx)
            });
        });
        if let Some(attempt) = self.session_admission.attempts.get_mut(&key) {
            attempt.phase = Phase::Checking;
            attempt.task = Some(task);
        }
    }

    fn wait_for_session_admission(
        &mut self,
        directory: String,
        token: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = directory.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(ADMISSION_WAIT).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.current_admission(&directory, token).is_some() {
                    let admitted = directory_admitted(&this.topology, &directory);
                    if !admitted && this.session_admission.attempts[&directory].deliberate {
                        this.admission_failed_notice(window, cx);
                    }
                    this.finish_session_admission(&directory, admitted, window, cx);
                }
            });
        });
        if let Some(attempt) = self.session_admission.attempts.get_mut(&key) {
            attempt.phase = Phase::Waiting;
            attempt.task = Some(task);
        }
    }

    fn finish_session_admission(
        &mut self,
        directory: &str,
        admitted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt = self.session_admission.attempts.remove(directory);
        if self.session_admission.prompt.as_deref() == Some(directory) {
            self.close_trust_prompt(window, cx);
        }
        if admitted
            && let Some((id, revision)) = attempt.and_then(|attempt| attempt.navigation)
            && revision == self.navigation_revision
            && self.session_admitted(&id)
        {
            self.continue_admitted_navigation(id, revision, window, cx);
        }
        self.show_next_trust_prompt(window, cx);
        cx.notify();
    }

    /// Parks a selection that lost admission; later navigation supersedes it.
    pub(crate) fn reconcile_session_admission(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let invalid: Vec<_> = self
            .session_admission
            .attempts
            .iter()
            .filter(|(directory, attempt)| {
                self.current_admission(directory, attempt.token).is_none()
            })
            .map(|(directory, _)| directory.clone())
            .collect();
        for directory in invalid {
            self.finish_session_admission(&directory, false, window, cx);
        }
        let admitted: Vec<_> = self
            .session_admission
            .attempts
            .keys()
            .filter(|directory| directory_admitted(&self.topology, directory))
            .cloned()
            .collect();
        for directory in admitted {
            self.finish_session_admission(&directory, true, window, cx);
        }
        if let Some((id, revision)) = self.session_admission.parked.clone() {
            if self.selected.is_some()
                || self.navigation_revision != revision
                || self.sessions.get(&id).is_none()
            {
                self.session_admission.parked = None;
            } else if self.session_admitted(&id) {
                self.session_admission.parked = None;
                self.continue_admitted_navigation(id, revision, window, cx);
            }
        }
        if let Some(id) = self.selected.clone()
            && self.sessions.get(&id).is_some()
            && !self.session_admitted(&id)
        {
            self.close_selected_session(window, cx);
            self.session_admission.parked = Some((id, self.navigation_revision));
        }
    }

    fn admission_failed_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_error(
            "directoryExplorerDialog.toast.failedToAddProject",
            None,
            window,
            cx,
        );
    }

    fn continue_admitted_navigation(
        &mut self,
        session: SessionId,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if revision != self.navigation_revision || !self.session_admitted(&session) {
            return;
        }
        self.session_admission.deferred_navigation = None;
        if !window.has_active_dialog(cx) && !window.has_active_sheet(cx) {
            self.select_admitted_session(session, window, cx);
            return;
        }
        // Activating a transcript also resets text selection. Preserve the whole
        // modal interaction until its authoritative overlay owner is dismissed.
        let owner = cx.weak_entity();
        let observer = window.observe_modal_state(cx, move |covered, window, cx| {
            if !covered {
                let _ = owner.update(cx, |this, cx| {
                    this.resume_admitted_navigation(window, cx);
                });
            }
        });
        self.session_admission.deferred_navigation = Some(DeferredNavigation {
            runtime: self.runtime.clone(),
            session,
            revision,
            _observer: observer,
        });
    }

    fn resume_admitted_navigation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) || window.has_active_sheet(cx) {
            return;
        }
        let Some(pending) = self.session_admission.deferred_navigation.take() else {
            return;
        };
        if pending.runtime == self.runtime
            && pending.revision == self.navigation_revision
            && self.session_admitted(&pending.session)
        {
            self.select_admitted_session(pending.session, window, cx);
        }
    }

    fn close_trust_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_admission.prompt = None;
        if let Some(dialog) = self.session_admission.prompt_dialog.take() {
            window.close_dialog_by_handle(&dialog, cx);
        }
    }

    fn show_next_trust_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_admission.prompt.is_some() {
            return;
        }
        while let Some(directory) = self.session_admission.prompts.pop_front() {
            let Some(attempt) = self.session_admission.attempts.get(&directory) else {
                continue;
            };
            let token = attempt.token;
            if self.current_admission(&directory, token).is_none() {
                continue;
            }
            self.session_admission.prompt = Some(directory.clone());
            // Reassess on reaching the front: another worktree may now be trusted.
            let Some(connection) = self.connection.as_ref() else {
                return;
            };
            let Some(client) = connection.client.clone() else {
                return;
            };
            let read_directory = directory.clone();
            let slots = self.session_admission.slots.0.clone();
            let task = connection.network.run(async move {
                let _permit = slots
                    .acquire_owned()
                    .await
                    .map_err(|_| TransportError::Cancelled)?;
                client.folder_trust(&read_directory).await
            });
            let key = directory.clone();
            let read = cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.current_admission(&directory, token).is_none() {
                        return;
                    }
                    match result {
                        Some(Ok(assessment)) if assessment.trusted => {
                            this.session_admission.prompt = None;
                            this.register_session_directory(directory, token, window, cx);
                            this.show_next_trust_prompt(window, cx);
                        }
                        Some(Ok(assessment)) => {
                            this.open_trust_prompt(directory, assessment, window, cx)
                        }
                        _ => {
                            this.session_admission.prompt = None;
                            this.retry_session_admission(directory, token, window, cx);
                            this.show_next_trust_prompt(window, cx);
                        }
                    }
                });
            });
            if let Some(attempt) = self.session_admission.attempts.get_mut(&key) {
                attempt.task = Some(read);
            }
            return;
        }
    }

    fn open_trust_prompt(
        &mut self,
        directory: String,
        assessment: TrustAssessment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attempt) = self.session_admission.attempts.get_mut(&directory) else {
            return;
        };
        if !attempt.deliberate
            && self
                .session_admission
                .declined
                .contains(&(assessment.directory.clone(), assessment.fingerprint.clone()))
        {
            self.session_admission.prompt = None;
            self.finish_session_admission(&directory, false, window, cx);
            return;
        }
        let token = attempt.token;
        attempt.phase = Phase::Prompt(assessment);
        let workspace = cx.weak_entity();
        let dialog = window.open_dialog_with_handle(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let state = this.read(cx);
            let Some(attempt) = state
                .session_admission
                .attempts
                .get(&directory)
                .filter(|attempt| attempt.token == token)
            else {
                return dialog;
            };
            let (shown, saving) = match &attempt.phase {
                Phase::Prompt(shown) => (shown, false),
                Phase::Saving(shown) => (shown, true),
                _ => return dialog,
            };
            let shown_fingerprint = shown.fingerprint.clone();
            let shown_directory = shown.directory.clone();
            let strings = locale::strings(cx);
            let palette = cx.global::<Palette>();
            let direction = strings.direction();
            let skip = workspace.clone();
            let trust = workspace.clone();
            let closed = workspace.clone();
            let key = directory.clone();
            let skip_key = directory.clone();
            let trust_key = directory.clone();
            let plugins: Vec<_> = shown
                .items
                .iter()
                .filter(|item| {
                    matches!(
                        item.kind,
                        TrustKind::Plugin | TrustKind::Package | TrustKind::Config
                    )
                })
                .collect();
            let commands: Vec<_> = shown
                .items
                .iter()
                .filter(|item| {
                    !matches!(
                        item.kind,
                        TrustKind::Plugin | TrustKind::Package | TrustKind::Config
                    )
                })
                .collect();
            let group = |heading: &str, items: &[&TrustItem]| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_color(palette.muted_foreground)
                            .child(strings.get(heading)),
                    )
                    .child(
                        div()
                            .p(px(10.0))
                            .rounded(px(6.0))
                            .bg(palette.foreground.opacity(0.04))
                            .children(items.iter().map(|item| {
                                div().child(trust_item_label(item, strings)).when(
                                    !matches!(
                                        item.kind,
                                        TrustKind::Plugin | TrustKind::Package | TrustKind::Config
                                    ),
                                    |this| this.child(format!("  {}", item.detail)),
                                )
                            })),
                    )
            };
            dialog
                .w(px(480.0))
                .close_button(false)
                .keyboard(!saving)
                .overlay_closable(!saving)
                .on_ok(|_, _, _| false)
                .on_close(move |_, window, cx| {
                    let _ = closed.update(cx, |this, cx| {
                        if this.session_admission.prompt.as_deref() == Some(key.as_str())
                            && this
                                .session_admission
                                .attempts
                                .get(&key)
                                .is_some_and(|attempt| attempt.token == token)
                        {
                            this.session_admission.prompt_dialog = None;
                            this.decline_trust_prompt(&key, token, window, cx);
                        }
                    });
                })
                .title(strings.get("projects.opencodeTrust.title"))
                .child(
                    div()
                        .text_color(palette.muted_foreground)
                        .child(strings.format(
                            "projects.opencodeTrust.description",
                            &[("path", &shown.directory)],
                        )),
                )
                .child(
                    div()
                        .id("session-folder-trust-items")
                        .max_h(px(280.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .when(!plugins.is_empty(), |this| {
                            this.child(group("projects.opencodeTrust.plugins", &plugins))
                        })
                        .when(!commands.is_empty(), |this| {
                            this.child(group("projects.opencodeTrust.commands", &commands))
                        })
                        .child(
                            div()
                                .text_color(palette.muted_foreground)
                                .child(strings.get("projects.opencodeTrust.paused")),
                        ),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            Button::new("session-folder-trust-skip")
                                .label(strings.get("projects.opencodeTrust.skip"))
                                .disabled(saving)
                                .on_click(move |_, window, cx| {
                                    let _ = skip.update(cx, |this, cx| {
                                        this.decline_trust_prompt(&skip_key, token, window, cx)
                                    });
                                }),
                        )
                        .child(
                            Button::new("session-folder-trust-approve")
                                .primary()
                                .label(strings.get("projects.opencodeTrust.trust"))
                                .disabled(saving)
                                .on_click(move |_, window, cx| {
                                    let _ = trust.update(cx, |this, cx| {
                                        this.save_trust_prompt(
                                            &trust_key,
                                            token,
                                            &shown_directory,
                                            &shown_fingerprint,
                                            window,
                                            cx,
                                        )
                                    });
                                }),
                        ),
                )
        });
        self.session_admission.prompt_dialog = Some(dialog);
        cx.notify();
    }

    fn decline_trust_prompt(
        &mut self,
        directory: &str,
        token: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attempt) = self
            .session_admission
            .attempts
            .get(directory)
            .filter(|attempt| attempt.token == token)
        else {
            return;
        };
        let Phase::Prompt(assessment) = &attempt.phase else {
            return;
        };
        self.session_admission
            .declined
            .insert((assessment.directory.clone(), assessment.fingerprint.clone()));
        self.finish_session_admission(directory, false, window, cx);
    }

    fn save_trust_prompt(
        &mut self,
        directory: &str,
        token: u64,
        shown_directory: &str,
        shown_fingerprint: &Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attempt) = self
            .session_admission
            .attempts
            .get(directory)
            .filter(|attempt| attempt.token == token)
        else {
            return;
        };
        let Phase::Prompt(shown) = &attempt.phase else {
            return;
        };
        // A queued click from an older render cannot approve a new assessment.
        if shown.directory != shown_directory || &shown.fingerprint != shown_fingerprint {
            return;
        }
        if self.current_admission(directory, token).is_none() {
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let shown = shown.clone();
        let original = shown.clone();
        let slots = self.session_admission.slots.0.clone();
        let task = connection.network.run(async move {
            let _permit = slots
                .acquire_owned()
                .await
                .map_err(|_| TransportError::Cancelled)?;
            client.trust_folder(&shown).await
        });
        let directory = directory.to_owned();
        let key = directory.clone();
        let save = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.current_admission(&directory, token).is_none() {
                    return;
                }
                match result {
                    Some(Ok(TrustAnswer::Saved)) => {
                        this.close_trust_prompt(window, cx);
                        this.register_session_directory(directory, token, window, cx);
                        this.show_next_trust_prompt(window, cx);
                    }
                    Some(Ok(TrustAnswer::Changed(assessment))) => {
                        if let Some(attempt) = this.session_admission.attempts.get_mut(&directory) {
                            attempt.phase = Phase::Prompt(assessment);
                        }
                    }
                    _ => {
                        if let Some(attempt) = this.session_admission.attempts.get_mut(&directory)
                            && let Phase::Saving(shown) = &attempt.phase
                        {
                            attempt.phase = Phase::Prompt(shown.clone());
                        }
                        this.push_error("projects.opencodeTrust.saveFailed", None, window, cx);
                    }
                }
                cx.notify();
            });
        });
        if let Some(attempt) = self.session_admission.attempts.get_mut(&key) {
            attempt.phase = Phase::Saving(original);
            attempt.task = Some(save);
        }
        cx.notify();
    }
}

fn retry_delay(attempt: &mut Attempt) -> Option<Duration> {
    if !attempt.automatic {
        return None;
    }
    let delay = RETRY_DELAYS.get(attempt.retries).copied()?;
    attempt.retries += 1;
    Some(delay)
}

fn trust_item_label(item: &TrustItem, strings: &locale::Strings) -> gpui_kit::SharedString {
    let key = match item.kind {
        TrustKind::Plugin => {
            return if item.name.is_empty() {
                item.source.clone().into()
            } else {
                item.name.clone().into()
            };
        }
        TrustKind::Package => "projects.opencodeTrust.kind.package",
        TrustKind::Config => "projects.opencodeTrust.kind.config",
        TrustKind::Mcp => "projects.opencodeTrust.kind.mcp",
        TrustKind::Lsp => "projects.opencodeTrust.kind.lsp",
        TrustKind::Formatter => "projects.opencodeTrust.kind.formatter",
        TrustKind::Shell => "projects.opencodeTrust.kind.shell",
    };
    strings.format(key, &[("name", &item.name), ("path", &item.source)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::dialog::Dialog;
    use gpui_kit::component::input::{Input, InputState};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{App, AppContext, ClickEvent, ElementId, Focusable, TestSupportExt};
    use openchamber_native_core::ids::ProjectId;
    use openchamber_native_core::ownership::{ProjectWorktrees, WorktreeInfo};
    use openchamber_native_core::session::{SessionPatch, SessionRecord};
    use openchamber_native_core::session_index::SessionIndex;

    fn attempt(automatic: bool) -> Attempt {
        Attempt {
            token: 1,
            runtime: RuntimeId::from("synthetic"),
            generation: 1,
            sessions: HashSet::from([SessionId::from("one")]),
            deliberate: !automatic,
            automatic,
            navigation: None,
            retries: 0,
            phase: Phase::Checking,
            task: None,
        }
    }

    fn open_prompt(
        this: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> DialogLayerHandle {
        this.session_admission.next_token += 1;
        let mut pending = attempt(true);
        pending.token = this.session_admission.next_token;
        this.session_admission
            .attempts
            .insert("/original".into(), pending);
        this.session_admission.prompt = Some("/original".into());
        this.open_trust_prompt(
            "/original".into(),
            TrustAssessment {
                directory: "/canonical".into(),
                trusted: false,
                fingerprint: Some("shown-fingerprint".into()),
                items: Vec::new(),
            },
            window,
            cx,
        );
        this.session_admission
            .prompt_dialog
            .clone()
            .expect("trust owns its opening")
    }

    #[gpui_kit::test]
    fn admission_dismissal_under_a_newer_modal_preserves_focus_and_its_return_chain(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        enum Completion {
            Finished,
            Saved,
            Reset,
        }
        for completion in [Completion::Finished, Completion::Saved, Completion::Reset] {
            let (window, workspace) = crate::workspace_tests::open(cx);
            let (trust, original_focus) = cx
                .update_window(window, |_, window, cx| {
                    cx.set_reduce_motion(true);
                    workspace.update(cx, |this, cx| {
                        let focus = this.composer.read(cx).focus_handle(cx);
                        focus.focus(window, cx);
                        (open_prompt(this, window, cx), focus)
                    })
                })
                .expect("test window");
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.press("tab", cx);
                window.render_frame(cx);
                assert_eq!(
                    window
                        .find(ElementId::Name("session-folder-trust-skip".into()))
                        .focused(),
                    Some(true)
                );

                let input = cx.new(|cx| InputState::new(window, cx));
                let newer_focus = input.read(cx).focus_handle(cx);
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog.title("Newer modal").child(
                        div()
                            .id("newer-modal")
                            .test_support()
                            .child(Input::new(&input)),
                    )
                });
                newer_focus.focus(window, cx);
                window.render_frame(cx);
                assert!(window.has_focused_input(cx));
                workspace.update(cx, |this, cx| {
                    match completion {
                        Completion::Finished => {
                            this.finish_session_admission("/original", true, window, cx)
                        }
                        Completion::Saved => {
                            let attempt = this
                                .session_admission
                                .attempts
                                .get_mut("/original")
                                .expect("attempt");
                            let Phase::Prompt(shown) = &attempt.phase else {
                                panic!("shown prompt");
                            };
                            attempt.phase = Phase::Saving(shown.clone());
                            // The successful save branch uses this dismissal before registration.
                            this.close_trust_prompt(window, cx);
                        }
                        Completion::Reset => this.reset_session_admission(window, cx),
                    }
                    this.finish_session_admission("/original", false, window, cx);
                    this.close_trust_prompt(window, cx);
                    assert!(this.session_admission.prompt_dialog.is_none());
                });
                assert_eq!(window.focused(cx), Some(newer_focus.clone()));
                assert!(
                    window.has_focused_input(cx),
                    "removing a covered layer retains the newer input"
                );
                assert!(
                    !window.close_dialog_by_handle(&trust, cx),
                    "repeated completion is a no-op"
                );
                window.render_frame(cx);
                assert!(
                    window
                        .try_find(ElementId::Name("newer-modal".into()))
                        .is_some()
                );
                assert!(
                    window
                        .try_find(ElementId::Name("session-folder-trust-skip".into()))
                        .is_none()
                );
                window.close_dialog(cx);
                assert!(
                    !window.has_active_dialog(cx),
                    "trust cannot remain below the newer modal"
                );
                assert_eq!(window.focused(cx), Some(original_focus));
            })
            .expect("test window");
        }
    }

    #[gpui_kit::test]
    fn admitted_navigation_preserves_a_newer_modals_input_focus(cx: &mut gpui_kit::TestAppContext) {
        for (already_selected, supersede, reset) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let (window, workspace) = crate::workspace_tests::open(cx);
            let id = SessionId::from("admitted-navigation");
            cx.update_window(window, |_, window, cx| {
                cx.set_reduce_motion(true);
                workspace.update(cx, |this, cx| {
                    this.sessions = SessionIndex::new();
                    this.topology.projects = vec![ConfiguredProject {
                        id: ProjectId::from("admitted"),
                        path: "/admitted".into(),
                    }];
                    let load = this.sessions.begin_load();
                    let _ = this
                        .sessions
                        .complete_load(load, vec![SessionRecord::new(id.clone(), "/admitted")]);
                    if already_selected {
                        this.select_admitted_session(id.clone(), window, cx);
                    }
                    open_prompt(this, window, cx);
                    this.session_admission
                        .attempts
                        .get_mut("/original")
                        .expect("trust retains the deliberate open")
                        .navigation = Some((id.clone(), this.navigation_revision));
                });
                let input = cx.new(|cx| InputState::new(window, cx));
                let focus = input.read(cx).focus_handle(cx);
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog.title("Newer modal").child(Input::new(&input))
                });
                focus.focus(window, cx);
                window.render_frame(cx);
                workspace.update(cx, |this, cx| {
                    let previous = this.selected.clone();
                    this.finish_session_admission("/original", true, window, cx);
                    assert_eq!(
                        this.selected, previous,
                        "covered navigation cannot activate a transcript or reset modal selection"
                    );
                    assert!(this.session_admission.deferred_navigation.is_some());
                });
                assert_eq!(
                    window.focused(cx),
                    Some(focus.clone()),
                    "navigation cannot take focus from the surviving modal"
                );
                window.render_frame(cx);
                assert_eq!(
                    window.focused(cx),
                    Some(focus),
                    "the surviving input remains focused after paint"
                );
                workspace.update(cx, |this, cx| {
                    if supersede {
                        this.show_new_session(window, cx);
                    }
                    if reset {
                        this.reset_session_admission(window, cx);
                    }
                });
                window.close_dialog(cx);
            })
            .expect("test window");
            cx.run_until_parked();
            cx.update_window(window, |_, _, cx| {
                workspace.update(cx, |this, _| {
                    assert!(this.session_admission.deferred_navigation.is_none());
                    assert_eq!(
                        this.selected.as_ref(),
                        (!supersede && !reset).then_some(&id),
                        "only an uncovered continuation with its original navigation owner resumes"
                    );
                });
            })
            .expect("test window");
        }
    }

    #[gpui_kit::test]
    fn removing_trust_keeps_a_newer_buttons_focus_across_repaint(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use std::cell::Cell;
        use std::rc::Rc;

        let (window, workspace) = crate::workspace_tests::open(cx);
        let pressed = Rc::new(Cell::new(0));
        let clicks = pressed.clone();
        cx.update_window(window, |_, window, cx| {
            cx.set_reduce_motion(true);
            workspace.update(cx, |this, cx| {
                open_prompt(this, window, cx);
            });
            window.open_dialog(cx, move |dialog, _, _| {
                let clicks = clicks.clone();
                dialog.close_button(false).child(
                    Button::new("newer-modal-button")
                        .label("Keep focus")
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                )
            });
            window.render_frame(cx);
            window.press("tab", cx);
            window.render_frame(cx);
            assert_eq!(
                window
                    .find(ElementId::Name("newer-modal-button".into()))
                    .focused(),
                Some(true)
            );
            let focused = window.focused(cx);
            workspace.update(cx, |this, cx| this.reset_session_admission(window, cx));
            window.render_frame(cx);
            assert_eq!(window.focused(cx), focused);
            assert_eq!(
                window
                    .find(ElementId::Name("newer-modal-button".into()))
                    .focused(),
                Some(true)
            );
            window.press("space", cx);
            assert_eq!(
                pressed.get(),
                1,
                "keyboard activation still reaches the surviving button"
            );
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn trust_cancel_and_skip_retire_their_opening_without_closing_its_replacement(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        for escape in [false, true] {
            let (window, workspace) = crate::workspace_tests::open(cx);
            let first = cx
                .update_window(window, |_, window, cx| {
                    cx.set_reduce_motion(true);
                    workspace.update(cx, |this, cx| open_prompt(this, window, cx))
                })
                .expect("test window");
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                if escape {
                    window.press("escape", cx);
                } else {
                    window.click(ElementId::Name("session-folder-trust-skip".into()), cx);
                }
                assert!(!window.has_active_dialog(cx));
                workspace.update(cx, |this, cx| {
                    assert!(this.session_admission.attempts.is_empty());
                    assert!(this.session_admission.prompt_dialog.is_none());
                    assert!(
                        this.session_admission
                            .declined
                            .contains(&("/canonical".into(), Some("shown-fingerprint".into()),))
                    );
                    this.session_admission.declined.clear();
                    let replacement = open_prompt(this, window, cx);
                    assert_ne!(first, replacement, "each opening has its own identity");
                    assert!(!window.close_dialog_by_handle(&first, cx));
                    assert_eq!(
                        this.session_admission.prompt_dialog.as_ref(),
                        Some(&replacement)
                    );
                });
                window.render_frame(cx);
                assert!(
                    window
                        .try_find(ElementId::Name("session-folder-trust-skip".into()))
                        .is_some()
                );
            })
            .expect("test window");
        }
    }

    #[gpui_kit::test]
    fn targeted_dialog_handles_are_root_scoped_and_stale_after_top_close_or_close_all(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (first_window, _) = crate::workspace_tests::open(cx);
        let first = cx
            .update_window(first_window, |_, window, cx| {
                window.open_dialog_with_handle(cx, |dialog, _, _| dialog.title("First"))
            })
            .expect("first window");
        let (second_window, _) = crate::workspace_tests::open(cx);
        cx.update_window(second_window, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Other window"));
            let focus = window.focused(cx);
            assert!(!window.close_dialog_by_handle(&first, cx));
            assert!(window.has_active_dialog(cx));
            assert_eq!(window.focused(cx), focus);
        })
        .expect("second window");
        cx.update_window(first_window, |_, window, cx| {
            window.close_dialog(cx);
            let replacement =
                window.open_dialog_with_handle(cx, |dialog, _, _| dialog.title("Replacement"));
            assert!(!window.close_dialog_by_handle(&first, cx));
            assert!(window.has_active_dialog(cx));
            window.close_all_dialogs(cx);
            window.open_dialog(cx, |dialog, _, _| dialog.title("After close all"));
            let focus = window.focused(cx);
            assert!(!window.close_dialog_by_handle(&replacement, cx));
            assert_eq!(window.focused(cx), focus);
            assert!(window.has_active_dialog(cx));
        })
        .expect("first window");
    }

    #[gpui_kit::test]
    fn targeted_dialog_decision_callbacks_can_open_a_newer_modal(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use std::cell::Cell;
        use std::rc::Rc;

        fn open_newer(_: &ClickEvent, window: &mut Window, cx: &mut App) -> bool {
            window.open_dialog(cx, |dialog, _, _| {
                dialog.child(
                    div()
                        .id("callback-modal")
                        .test_support()
                        .child("Newer modal"),
                )
            });
            true
        }

        for key in ["enter", "escape"] {
            let (window, workspace) = crate::workspace_tests::open(cx);
            let closed = Rc::new(Cell::new(0));
            let close_count = closed.clone();
            let handle = cx
                .update_window(window, |_, window, cx| {
                    cx.set_reduce_motion(true);
                    workspace
                        .read(cx)
                        .composer
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx);
                    window.open_dialog_with_handle(cx, move |dialog: Dialog, _, _| {
                        let close_count = close_count.clone();
                        dialog
                            .title("Decision")
                            .on_ok(open_newer)
                            .on_cancel(open_newer)
                            .on_close(move |_, _, _| close_count.set(close_count.get() + 1))
                    })
                })
                .expect("test window");
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.press(key, cx);
                assert_eq!(
                    closed.get(),
                    1,
                    "normal dismissal invokes its close callback once"
                );
                assert!(!window.close_dialog_by_handle(&handle, cx));
                window.render_frame(cx);
                assert!(
                    window
                        .try_find(ElementId::Name("callback-modal".into()))
                        .is_some()
                );
                window.close_dialog(cx);
                assert!(!window.has_active_dialog(cx));
                assert!(
                    workspace
                        .read(cx)
                        .composer
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
            })
            .expect("test window");
        }
    }

    #[gpui_kit::test]
    fn modal_observation_reconciles_before_activation_closes_and_cancels_on_drop(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;

        for (close_before_activation, cancel) in [(true, false), (true, true), (false, false)] {
            let (window, _) = crate::workspace_tests::open(cx);
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = events.clone();
            let _subscription = cx
                .update_window(window, |_, window, cx| {
                    // Queue a root notification before Window::observe queues its activation.
                    let dialog = window.open_dialog_with_handle(cx, |dialog, _, _| {
                        dialog.title("Covered at registration")
                    });
                    let subscription = window.observe_modal_state(cx, move |covered, _, _| {
                        observed.borrow_mut().push(covered);
                    });
                    if close_before_activation {
                        assert!(window.close_dialog_by_handle(&dialog, cx));
                    }
                    if cancel {
                        drop(subscription);
                        None
                    } else {
                        Some(subscription)
                    }
                })
                .expect("test window");
            cx.run_until_parked();
            let expected = if close_before_activation && !cancel {
                vec![false]
            } else {
                Vec::new()
            };
            assert_eq!(
                *events.borrow(),
                expected,
                "activation reconciles a missed transition exactly once; cancellation and unchanged state stay silent"
            );
        }
    }

    #[gpui_kit::test]
    fn admitted_navigation_after_legacy_confirm_keeps_composer_focus_after_the_delay(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::component::dialog::ANIMATION_DURATION;

        let (window, workspace) = crate::workspace_tests::open(cx);
        let id = SessionId::from("admitted-after-confirm");
        let (background_focus, composer_focus) = cx
            .update_window(window, |_, window, cx| {
                cx.set_reduce_motion(true);
                workspace.update(cx, |this, _| {
                    this.sessions = SessionIndex::new();
                    this.topology.projects = vec![ConfiguredProject {
                        id: ProjectId::from("admitted"),
                        path: "/admitted".into(),
                    }];
                    let load = this.sessions.begin_load();
                    let _ = this
                        .sessions
                        .complete_load(load, vec![SessionRecord::new(id.clone(), "/admitted")]);
                });
                // The workspace's rendered focus scope is a live non-composer return target.
                let background_focus = workspace.read(cx).focus_handle.clone();
                let composer_focus = workspace.read(cx).composer.read(cx).focus_handle(cx);
                assert_ne!(background_focus, composer_focus);
                window.render_frame(cx);
                background_focus.focus(window, cx);
                window.render_frame(cx);
                assert!(background_focus.is_focused(window));
                window.open_dialog(cx, |dialog, _, _| {
                    dialog.title("Legacy confirmation").close_button(false)
                });
                window.render_frame(cx);
                workspace.update(cx, |this, cx| {
                    let mut pending = attempt(false);
                    pending.runtime = this.runtime.clone();
                    pending.sessions = HashSet::from([id.clone()]);
                    pending.phase = Phase::Registering;
                    pending.navigation = Some((id.clone(), this.navigation_revision));
                    this.session_admission
                        .attempts
                        .insert("/admitted".into(), pending);
                    this.finish_session_admission("/admitted", true, window, cx);
                    assert!(
                        this.selected.is_none(),
                        "covered admission keeps navigation pending"
                    );
                    assert!(this.session_admission.deferred_navigation.is_some());
                });
                window.press("enter", cx);
                assert!(!window.has_active_dialog(cx));
                (background_focus, composer_focus)
            })
            .expect("test window");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert_eq!(workspace.read(cx).selected.as_ref(), Some(&id));
            assert!(
                workspace
                    .read(cx)
                    .session_admission
                    .deferred_navigation
                    .is_none()
            );
            assert!(
                composer_focus.is_focused(window),
                "the actual admission continuation resumed"
            );
        })
        .expect("test window");
        cx.executor().advance_clock(*ANIMATION_DURATION);
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(
                composer_focus.is_focused(window),
                "the delayed return cannot override resumed navigation"
            );
            assert!(!background_focus.is_focused(window));
            window.render_frame(cx);
            assert!(
                composer_focus.is_focused(window),
                "composer focus survives repaint"
            );
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn modal_observation_reports_only_coverage_transitions_in_its_own_window(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::component::notification::Notification;
        use std::cell::RefCell;
        use std::rc::Rc;

        let (window, _) = crate::workspace_tests::open(cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let subscription = cx
            .update_window(window, |_, actual_window, cx| {
                actual_window.observe_modal_state(cx, move |covered, actual_window, cx| {
                    assert_eq!(actual_window.window_handle(), window);
                    assert_eq!(
                        covered,
                        actual_window.has_active_dialog(cx) || actual_window.has_active_sheet(cx)
                    );
                    observed.borrow_mut().push(covered);
                })
            })
            .expect("test window");
        cx.run_until_parked();
        assert!(events.borrow().is_empty(), "there is no initial callback");

        let (other_window, _) = crate::workspace_tests::open(cx);
        cx.update_window(other_window, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Other window"));
        })
        .expect("other window");
        cx.update_window(window, |_, window, cx| {
            window.push_notification(Notification::error("Toast"), cx);
            window.clear_notifications(cx);
        })
        .expect("test window");
        cx.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "other windows and toasts do not change coverage"
        );

        let lower = cx
            .update_window(window, |_, window, cx| {
                window.open_dialog_with_handle(cx, |dialog, _, _| dialog.title("Lower"))
            })
            .expect("test window");
        cx.run_until_parked();
        assert_eq!(*events.borrow(), [true]);
        cx.update_window(window, |_, window, cx| {
            window.open_sheet(cx, |sheet, _, _| sheet.child("Sheet"));
            window.open_dialog(cx, |dialog, _, _| dialog.title("Upper"));
        })
        .expect("test window");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(window.close_dialog_by_handle(&lower, cx));
            window.close_dialog(cx);
            assert!(window.has_active_sheet(cx));
        })
        .expect("test window");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [true],
            "a remaining sheet still owns interaction"
        );
        cx.update_window(window, |_, window, cx| {
            window.close_sheet(cx);
            window.open_dialog(cx, |dialog, _, _| dialog.title("Replacement"));
        })
        .expect("test window");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [true],
            "close/open in one update remains covered"
        );
        cx.update_window(window, |_, window, cx| window.close_dialog(cx))
            .expect("test window");
        cx.run_until_parked();
        assert_eq!(*events.borrow(), [true, false]);

        drop(subscription);
        cx.update_window(window, |_, window, cx| {
            window.open_sheet(cx, |sheet, _, _| sheet.child("After unsubscribe"));
        })
        .expect("test window");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [true, false],
            "dropping the subscription ends delivery"
        );
    }

    #[gpui_kit::test]
    fn modal_observation_rejects_queued_notifications_from_a_replaced_root(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::component::Root;
        use std::cell::RefCell;
        use std::rc::Rc;

        let (window, workspace) = crate::workspace_tests::open(cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = cx
            .update_window(window, |_, window, cx| {
                window.observe_modal_state(cx, move |covered, _, _| {
                    observed.borrow_mut().push(covered);
                })
            })
            .expect("test window");
        // Retain the old plugin owner so its queued notification can still arrive.
        let _old_root = cx
            .update_window(window, |_, window, cx| {
                let old_root = window.root::<Root>().flatten().expect("Kit root");
                window.open_dialog(cx, |dialog, _, _| dialog.title("Old root"));
                window.replace_root(cx, |window, cx| Root::new(workspace.clone(), window, cx));
                old_root
            })
            .expect("test window");
        cx.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "an old root cannot report current coverage"
        );
        cx.update_window(window, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("New root"));
        })
        .expect("test window");
        cx.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "the subscription cannot migrate to another root"
        );
    }

    #[gpui_kit::test]
    fn deferred_legacy_close_cannot_focus_a_removed_handled_dialog(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::component::dialog::ANIMATION_DURATION;
        use std::cell::Cell;
        use std::rc::Rc;

        for remove_target in [false, true] {
            let (window, workspace) = crate::workspace_tests::open(cx);
            let closed = Rc::new(Cell::new(0));
            let close_count = closed.clone();
            let (handled_a, a_focus, background_focus) = cx
                .update_window(window, |_, window, cx| {
                    cx.set_reduce_motion(true);
                    let background_focus = workspace.read(cx).composer.read(cx).focus_handle(cx);
                    background_focus.focus(window, cx);
                    let input = cx.new(|cx| InputState::new(window, cx));
                    let a_focus = input.read(cx).focus_handle(cx);
                    let handled_a = window.open_dialog_with_handle(cx, move |dialog, _, _| {
                        dialog
                            .title("A")
                            .close_button(false)
                            .child(Input::new(&input))
                    });
                    a_focus.focus(window, cx);
                    window.render_frame(cx);
                    window.open_dialog(cx, move |dialog, _, _| {
                        let close_count = close_count.clone();
                        dialog
                            .title("B")
                            .close_button(false)
                            .on_close(move |_, _, _| {
                                close_count.set(close_count.get() + 1);
                            })
                    });
                    window.render_frame(cx);
                    window.press("enter", cx);
                    assert_eq!(
                        closed.get(),
                        1,
                        "B confirmation scheduled its deferred return"
                    );
                    (handled_a, a_focus, background_focus)
                })
                .expect("test window");
            cx.run_until_parked();
            let expected_focus = cx
                .update_window(window, |_, window, cx| {
                    if !remove_target {
                        return a_focus.clone();
                    }
                    assert!(window.close_dialog_by_handle(&handled_a, cx));
                    let input = cx.new(|cx| InputState::new(window, cx));
                    let c_focus = input.read(cx).focus_handle(cx);
                    window.open_dialog(cx, move |dialog, _, _| {
                        dialog
                            .title("C")
                            .close_button(false)
                            .child(Input::new(&input))
                    });
                    c_focus.focus(window, cx);
                    window.render_frame(cx);
                    c_focus
                })
                .expect("test window");
            cx.executor().advance_clock(*ANIMATION_DURATION);
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                assert_eq!(
                    window.focused(cx),
                    Some(expected_focus.clone()),
                    "a surviving target receives legacy focus return; removed A cannot steal C focus"
                );
                window.render_frame(cx);
                assert!(expected_focus.is_focused(window), "focus survives the next paint");
                window.close_dialog(cx);
                assert!(!window.has_active_dialog(cx));
                assert_eq!(
                    window.focused(cx),
                    Some(background_focus),
                    "C cannot inherit the removed dialog's return target"
                );
            })
            .expect("test window");
        }
    }

    #[gpui_kit::test]
    fn an_older_legacy_close_cannot_consume_a_newer_pending_focus_return(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::component::dialog::ANIMATION_DURATION;

        let (window, _) = crate::workspace_tests::open(cx);
        let a_focus = cx
            .update_window(window, |_, window, cx| {
                cx.set_reduce_motion(true);
                let input = cx.new(|cx| InputState::new(window, cx));
                let a_focus = input.read(cx).focus_handle(cx);
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog
                        .title("A")
                        .close_button(false)
                        .child(Input::new(&input))
                });
                a_focus.focus(window, cx);
                window.render_frame(cx);
                window.open_dialog(cx, |dialog, _, _| dialog.title("B").close_button(false));
                window.render_frame(cx);
                window.press("enter", cx);
                a_focus
            })
            .expect("test window");
        cx.run_until_parked();
        let delay = *ANIMATION_DURATION;
        let elapsed = delay / 2;
        cx.executor().advance_clock(elapsed);
        cx.update_window(window, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("C").close_button(false));
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .expect("test window");
        cx.run_until_parked();
        cx.executor().advance_clock(delay - elapsed);
        cx.run_until_parked();
        cx.update_window(window, |_, window, _| {
            assert!(
                !a_focus.is_focused(window),
                "B's consumed return cannot run before C's delay"
            );
        })
        .expect("test window");
        cx.executor().advance_clock(elapsed);
        cx.run_until_parked();
        cx.update_window(window, |_, window, _| {
            assert!(
                a_focus.is_focused(window),
                "B's callback cannot clear C's pending return"
            );
        })
        .expect("test window");
    }

    #[test]
    fn only_failed_automatic_attempts_have_the_three_upstream_retry_delays() {
        let mut automatic = attempt(true);
        assert_eq!(
            (0..4)
                .map(|_| retry_delay(&mut automatic))
                .collect::<Vec<_>>(),
            [
                Some(Duration::from_secs(1)),
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(4)),
                None
            ]
        );
        assert_eq!(retry_delay(&mut attempt(false)), None);
    }

    #[test]
    fn registration_append_survives_an_older_read_and_a_fresh_read_can_remove_it() {
        let project = ConfiguredProject {
            id: ProjectId::from("new"),
            path: "/new".into(),
        };
        let mut admission = SessionAdmission::default();
        admission.confirmed_projects.push((1, project.clone()));
        let mut old = Vec::new();
        admission.reconcile_projects(0, &mut old);
        assert_eq!(old, [project]);
        let mut fresh = Vec::new();
        admission.reconcile_projects(1, &mut fresh);
        assert!(
            fresh.is_empty(),
            "a confirmed append is not permanent registration authority"
        );
    }

    #[gpui_kit::test]
    fn unknown_history_stays_raw_and_never_starts_background_admission(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let id = SessionId::from("unknown-history");
                let load = this.sessions.begin_load();
                let update = this.sessions.complete_load(
                    load,
                    vec![SessionRecord::new(id.clone(), "/unknown-history")],
                );
                this.after_index_update(update, window, cx);
                assert!(
                    this.sessions.get(&id).is_some(),
                    "admission cannot erase raw history"
                );
                assert!(this.session_admission.attempts.is_empty());
                assert!(!this.open_sessions.contains_key(&id));
                this.ensure_open(&id, cx);
                assert!(
                    !this.open_sessions.contains_key(&id),
                    "unadmitted sessions cannot fetch history through an internal open"
                );
            });
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn declining_trust_ends_the_attempt_without_scheduling_a_failure_retry(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let mut pending = attempt(true);
                pending.phase = Phase::Prompt(TrustAssessment {
                    directory: "/canonical".into(),
                    trusted: false,
                    fingerprint: Some("declined-fingerprint".into()),
                    items: Vec::new(),
                });
                this.session_admission
                    .attempts
                    .insert("/original".into(), pending);
                this.decline_trust_prompt("/original", 1, window, cx);
                assert!(
                    this.session_admission.attempts.is_empty(),
                    "declines end their automatic retry owner"
                );
                assert!(
                    this.session_admission
                        .declined
                        .contains(&("/canonical".into(), Some("declined-fingerprint".into())))
                );
            });
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_moved_selection_waits_for_admission_and_yields_to_newer_navigation(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        for navigation in 0..3 {
            let navigate = navigation != 0;
            let (window, workspace) = crate::workspace_tests::open(cx);
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    this.sessions = SessionIndex::new();
                    this.topology.projects = vec![ConfiguredProject {
                        id: ProjectId::from("source"),
                        path: "/source".into(),
                    }];
                    let id = SessionId::from("moving");
                    let load = this.sessions.begin_load();
                    let _ = this
                        .sessions
                        .complete_load(load, vec![SessionRecord::new(id.clone(), "/source")]);
                    this.select_session(id.clone(), window, cx);
                    assert_eq!(this.selected.as_ref(), Some(&id));
                    let update = this.sessions.apply([SessionChange::Patched {
                        session_id: id.clone(),
                        patch: SessionPatch {
                            directory: Some("/destination".into()),
                            ..SessionPatch::default()
                        },
                    }]);
                    this.after_index_update(update, window, cx);
                    assert!(
                        this.selected.is_none(),
                        "a move cannot leave an unknown folder selected"
                    );
                    assert!(this.session_admission.parked.is_some());
                    match navigation {
                        1 => this.show_new_session(window, cx),
                        2 => this.toggle_archive_page(window, cx),
                        _ => {}
                    }
                    this.topology.projects.push(ConfiguredProject {
                        id: ProjectId::from("destination"),
                        path: "/destination".into(),
                    });
                    this.reconcile_session_admission(window, cx);
                    assert_eq!(this.selected.as_ref(), (!navigate).then_some(&id));
                    assert_eq!(
                        this.sessions
                            .get(&id)
                            .and_then(|session| session.directory.as_deref()),
                        Some("/destination")
                    );
                });
            })
            .expect("test window");
        }
    }

    #[gpui_kit::test]
    fn archive_rows_filter_admission_without_erasing_the_archived_index(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.topology.projects = vec![ConfiguredProject {
                    id: ProjectId::from("admitted"),
                    path: "/admitted".into(),
                }];
                let mut admitted = SessionRecord::new("archive-admitted", "/admitted");
                admitted.time.archived = Some(10);
                let mut hidden = SessionRecord::new("archive-hidden", "/unknown-archive");
                hidden.time.archived = Some(11);
                let load = this.sessions.begin_load();
                let update = this.sessions.complete_load(load, vec![admitted, hidden]);
                this.after_index_update(update, window, cx);
                this.toggle_archive_page(window, cx);
                assert_eq!(
                    this.sessions.archived().len(),
                    2,
                    "archive admission changes no raw records"
                );
            });
            window.render_frame(cx);
            assert!(
                window
                    .try_find(gpui_kit::ElementId::Name(
                        "archive-row:archive-admitted".into()
                    ))
                    .is_some()
            );
            assert!(
                window
                    .try_find(gpui_kit::ElementId::Name(
                        "archive-row:archive-hidden".into()
                    ))
                    .is_none()
            );
        })
        .expect("test window");
    }

    #[test]
    fn admission_requires_registered_ownership_and_keeps_managed_chats_separate() {
        let mut topology = Topology::default();
        topology.projects.push(ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/app".into(),
        });
        topology.worktrees.push(ProjectWorktrees {
            project_path: "/removed".into(),
            worktrees: vec![WorktreeInfo::new("/orphan")],
        });
        assert!(directory_admitted(&topology, "/app/src"));
        assert!(!directory_admitted(&topology, "/app-other"));
        assert!(!directory_admitted(&topology, "/orphan"));
        assert!(!directory_admitted(&topology, "/app/../outside"));
        topology.chat_roots =
            chat_roots::ChatRoots::from_home_info(Some("/home/test"), None, None, None);
        assert!(directory_admitted(
            &topology,
            "/home/test/.config/openchamber/chats/thread"
        ));
        assert!(!directory_admitted(&topology, "/home/test"));
    }
}
