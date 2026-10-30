//! Durable creation, exact observation and atomic first-message binding.

use gpui_kit::{Context, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{MessageId, PartId};
use openchamber_native_core::pending_requests::RequestKey;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::session_index::SessionChange;
use openchamber_native_core::transcript::{Message, MessageInfo, Part, Role};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::prompt::Prompt;
use openchamber_native_transport::session_create::{
    CreateSession, SessionError, SessionObservation,
};

use crate::connection::ConnectionState;
use crate::create_attempt::{CreateAttempt, CreationCheck, CreationPhase};
use crate::network::NetworkTask;
use crate::recovery_view::{RecoveryPhase, RecoveryRow};
use crate::send_attempt::{Admission, SendAttempt, SendOrigin, SessionKey};
use crate::session_mutations::CapturedPrompt;
use crate::workspace::Workspace;

enum CreatedAction {
    ObservePrompt,
    RetryPrompt(Vec<RequestKey>),
    /// The requests a retry supersedes and the context it found admitted.
    Dispatch(Vec<RequestKey>, Vec<MessageId>),
}

/// Auto always goes through OpenChamber's intercepted selection route. Ordinary
/// unchanged agent selections are omitted because OpenCode adds a notice for each.
pub(crate) fn first_prompt_selection(prompt: &Prompt, session: &SessionRecord) -> Prompt {
    let mut selected = prompt.clone();
    if selected.model.as_ref().is_some_and(|model| {
        !(model.provider_id == "openchamber" && model.model_id == "auto")
            && session.model.as_ref() == Some(model)
    }) {
        selected.model = None;
    }
    if selected.agent == session.agent {
        selected.agent = None;
    }
    selected
}

impl Workspace {
    pub(crate) fn can_create_connected(&self) -> bool {
        if self.selected.is_some() || !self.persistence_ready() {
            return false;
        }
        let Some(owner) = self.composer_key.as_ref() else {
            return false;
        };
        owner.runtime() == &self.runtime
            && matches!(owner.target(), DraftTarget::NewSession(_))
            && self
                .draft_projects
                .get(&self.runtime)
                .is_some_and(|directory| directory == owner.directory())
            && self
                .topology
                .projects
                .iter()
                .any(|project| project.path == owner.directory())
            && self.creations.can_begin(owner)
            && self.send_selection().is_some_and(|(directory, selection)| {
                directory == owner.directory() && selection.model.is_some()
            })
            && self.connection.as_ref().is_some_and(|connection| {
                matches!(connection.state, ConnectionState::Connected)
                    && connection.client.is_some()
            })
    }

    pub(crate) fn submit_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_create_connected() {
            return;
        }
        let Some(source) = self.composer_key.clone() else {
            return;
        };
        let Some(project) = self
            .topology
            .projects
            .iter()
            .find(|project| project.path == source.directory())
        else {
            return;
        };
        let project = project.id.to_string();
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        let draft = Draft {
            text: self.composer.read(cx).value().to_string(),
            selection: self.composer.read(cx).selected_range(),
        };
        if draft.text.trim().is_empty() {
            return;
        }
        let id = self.session_ids.next();
        let Ok(request) = CreateSession::new(
            id.clone(),
            source.directory(),
            selection.model.clone(),
            selection.agent_name.clone(),
        ) else {
            return;
        };
        let (message_id, created) = self.message_ids.next();
        let key = SessionKey {
            runtime: self.runtime.clone(),
            session: id,
        };
        let attempt = CreateAttempt {
            source: source.clone(),
            project,
            request,
            draft: draft.clone(),
            prompt: Prompt {
                message_id,
                text: draft.text,
                parts: Default::default(),
                model: selection.model,
                agent: selection.agent_name,
                delivery: None,
            },
            created,
            phase: CreationPhase::Preparing,
            navigation: Some(self.navigation_revision),
        };
        if !self.creations.insert(key.clone(), attempt) {
            return;
        }
        // The draft's own permission choice goes with its session; the next
        // draft starts from the Settings default again.
        if let Some(mode) = self.permission_modes.drafts.remove(&key.runtime) {
            self.permission_modes.creating.insert(key.clone(), mode);
        }
        if let Some(from) = self.selection_owner() {
            let to = openchamber_native_core::composer_selection::SelectionOwner {
                draft: DraftKey::new(
                    key.runtime.clone(),
                    source.directory(),
                    DraftTarget::Session(key.session.clone()),
                )
                .expect("creation has a valid directory"),
                project: from.project.clone(),
            };
            self.composer_selections.stage_creation(&from, to);
        }
        // Consume synchronously before yielding. Any subsequent typing, even
        // identical text, is a separate draft and is never filtered from disk.
        self.drafts.take(&source);
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        self.save_draft_preview(&source, "");
        let requests = self.requests_for_creation(&key, source.directory());
        self.start_creation_post(key, requests, window, cx);
    }

    fn start_creation_post(
        &mut self,
        key: SessionKey,
        requests: Vec<RequestKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready()
            || key.runtime != self.runtime
            || self.creation_tasks.contains_key(&key)
        {
            return;
        }
        let Some(attempt) = self.creations.get(&key) else {
            return;
        };
        if !matches!(
            attempt.phase,
            CreationPhase::Preparing | CreationPhase::Uncertain
        ) || !self.creation_project_registered(attempt)
        {
            return;
        }
        let request = attempt.request.clone();
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let executor = connection.network.handle();
        let Some((stamp, saved)) = self.persistence_barrier(cx) else {
            self.creations.phase(&key, CreationPhase::Uncertain);
            cx.notify();
            return;
        };
        let task_key = key.clone();
        let task =
            cx.spawn_in(window, async move |this, cx| {
                let saved = saved
                    .await
                    .unwrap_or(Err(crate::journal_worker::JournalFailure::WorkerStopped));
                let dispatch = this.update_in(cx, |this, _, cx| {
                    let durable =
                        this.record_persistence(stamp, &saved, cx) && this.persistence_ready();
                    let eligible = this.creations.get(&key).is_some_and(|attempt| {
                        matches!(
                            attempt.phase,
                            CreationPhase::Preparing | CreationPhase::Uncertain
                        )
                    });
                    if !durable || !eligible {
                        if eligible {
                            this.creations.phase(&key, CreationPhase::Uncertain);
                        }
                        this.creation_tasks.remove(&key);
                        cx.notify();
                        return false;
                    }
                    this.creations.phase(&key, CreationPhase::Creating);
                    cx.notify();
                    true
                });
                if !matches!(dispatch, Ok(true)) {
                    return;
                }
                let captured = client.clone();
                let result = NetworkTask::spawn_on(&executor, async move {
                    captured.create_session(&request).await
                })
                .await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.creation_tasks.remove(&key);
                    if this
                        .creations
                        .get(&key)
                        .is_none_or(|attempt| attempt.phase == CreationPhase::Deleted)
                    {
                        return;
                    }
                    match result {
                        Some(Ok(session)) => {
                            this.creations.phase(&key, CreationPhase::Confirmed);
                            this.bind_created_session(
                                &key,
                                session,
                                client,
                                executor,
                                requests,
                                (window, cx),
                            );
                        }
                        Some(Err(error))
                            if matches!(
                                error.source,
                                SessionError::IdentityMismatch | SessionError::LocationMismatch
                            ) =>
                        {
                            this.creations.phase(&key, CreationPhase::Mismatch);
                        }
                        _ => {
                            this.creations.phase(&key, CreationPhase::Uncertain);
                            // Observation is allowed; continuation still requires a click.
                            this.check_creation(&key, false, window, cx);
                        }
                    }
                    this.queue_persistence(cx);
                    cx.notify();
                });
            });
        self.creation_tasks.insert(task_key, task);
        cx.notify();
    }

    fn creation_project_registered(&self, attempt: &CreateAttempt) -> bool {
        self.topology.projects.iter().any(|project| {
            project.id.as_str() == attempt.project && project.path == attempt.request.directory()
        })
    }

    pub(crate) fn check_creation(
        &mut self,
        key: &SessionKey,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key.runtime != self.runtime
            || !self.persistence_ready()
            || self.creation_tasks.contains_key(key)
        {
            return;
        }
        let Some(attempt) = self.creations.get(key) else {
            return;
        };
        if matches!(
            attempt.phase,
            CreationPhase::Preparing | CreationPhase::Creating | CreationPhase::Deleted
        ) {
            return;
        }
        if retry
            && (!self.persistence_ready()
                || !(attempt.phase == CreationPhase::Confirmed
                    || (attempt.phase == CreationPhase::Uncertain
                        && self.creations.checks.get(key) == Some(&CreationCheck::NotObserved))))
        {
            return;
        }
        let request = attempt.request.clone();
        let requests = if retry {
            self.requests_for_creation(key, request.directory())
        } else {
            Vec::new()
        };
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let executor = connection.network.handle();
        let captured = client.clone();
        let lookup = connection.network.run(async move {
            captured
                .lookup_session(request.id(), request.directory())
                .await
        });
        self.creations
            .checks
            .insert(key.clone(), CreationCheck::Checking);
        let key = key.clone();
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = lookup.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.creation_tasks.remove(&key);
                this.creations.checks.remove(&key);
                let Some(attempt) = this.creations.get(&key) else {
                    return;
                };
                if attempt.phase == CreationPhase::Deleted {
                    return;
                }
                let confirmed = matches!(
                    attempt.phase,
                    CreationPhase::Confirming | CreationPhase::Confirmed
                );
                match result {
                    Some(Ok(SessionObservation::Found(session))) => {
                        this.creations.phase(&key, CreationPhase::Confirmed);
                        if retry {
                            this.bind_created_session(
                                &key,
                                *session,
                                client,
                                executor,
                                requests,
                                (window, cx),
                            );
                        } else if key.runtime == this.runtime {
                            let update = this.sessions.apply([SessionChange::Created(*session)]);
                            this.after_index_update(update, window, cx);
                        }
                        if !retry {
                            this.persist_creation_confirmation(&key, window, cx);
                        }
                    }
                    Some(Ok(SessionObservation::NotObserved)) if confirmed => {
                        this.mark_creation_deleted(&key, cx)
                    }
                    Some(Ok(SessionObservation::NotObserved)) => {
                        this.creations
                            .checks
                            .insert(key.clone(), CreationCheck::NotObserved);
                        if retry
                            && this
                                .creations
                                .get(&key)
                                .is_some_and(|attempt| attempt.phase == CreationPhase::Uncertain)
                        {
                            this.start_creation_post(key.clone(), requests, window, cx);
                        }
                    }
                    Some(Err(SessionError::IdentityMismatch | SessionError::LocationMismatch)) => {
                        // Keep established existence even if a later response is unsafe.
                        if !confirmed {
                            this.creations.phase(&key, CreationPhase::Mismatch);
                        }
                        this.creations
                            .checks
                            .insert(key.clone(), CreationCheck::Failed);
                    }
                    _ => {
                        this.creations
                            .checks
                            .insert(key.clone(), CreationCheck::Failed);
                    }
                }
                this.queue_persistence(cx);
                cx.notify();
            });
        });
        self.creation_tasks.insert(task_key, task);
        cx.notify();
    }

    fn persist_creation_confirmation(
        &mut self,
        key: &SessionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.creations.phase(key, CreationPhase::Confirming) {
            return;
        }
        let Some((stamp, saved)) = self.persistence_barrier(cx) else {
            return;
        };
        let key = key.clone();
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let saved = saved
                .await
                .unwrap_or(Err(crate::journal_worker::JournalFailure::WorkerStopped));
            let _ = this.update_in(cx, |this, _, cx| {
                this.creation_tasks.remove(&key);
                if this.record_persistence(stamp, &saved, cx)
                    && this
                        .creations
                        .get(&key)
                        .is_some_and(|attempt| attempt.phase == CreationPhase::Confirming)
                {
                    this.creations.phase(&key, CreationPhase::Confirmed);
                }
                cx.notify();
            });
        });
        self.creation_tasks.insert(task_key, task);
    }

    fn bind_created_session(
        &mut self,
        key: &SessionKey,
        session: SessionRecord,
        client: ServerClient,
        executor: tokio::runtime::Handle,
        requests: Vec<RequestKey>,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let (window, cx) = context;
        let Some(attempt) = self
            .creations
            .get(key)
            .filter(|attempt| attempt.phase == CreationPhase::Confirmed)
            .cloned()
        else {
            return;
        };
        if !self.send_attempts.can_send(key) {
            return;
        }
        let Some(owner) = DraftKey::new(
            key.runtime.clone(),
            attempt.request.directory(),
            DraftTarget::Session(key.session.clone()),
        ) else {
            return;
        };
        let dispatched = first_prompt_selection(&attempt.prompt, &session);
        let selection_owner = openchamber_native_core::composer_selection::SelectionOwner {
            draft: owner.clone(),
            project: Some(openchamber_native_core::ids::ProjectId::from(
                attempt.project.clone(),
            )),
        };
        let selection = openchamber_native_core::send_config::Selection {
            model: attempt.prompt.model.clone(),
            agent_name: attempt.prompt.agent.clone(),
        };
        let optimistic = Message {
            info: MessageInfo {
                id: attempt.prompt.message_id.clone(),
                role: Role::User,
                created: attempt.created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: vec![Part::text(
                PartId::new(format!("{}-local-text", attempt.prompt.message_id)),
                attempt.prompt.text.clone(),
            )],
        };
        let send = SendAttempt {
            origin: SendOrigin::Created,
            owner,
            draft: attempt.draft,
            prompt: attempt.prompt,
            optimistic,
            admission: Admission::Preparing,
        };
        if !self.send_attempts.begin(key.clone(), send) {
            return;
        }
        let source = if self.drafts.get(&attempt.source).is_some() {
            openchamber_native_core::composer_selection::CreationSource::Continued
        } else {
            openchamber_native_core::composer_selection::CreationSource::Consumed
        };
        self.composer_selections
            .commit_creation(selection_owner, selection, source);
        self.creations.remove(key);
        if key.runtime == self.runtime {
            self.save_composer_draft(cx);
            let update = self
                .sessions
                .apply([SessionChange::Created(session.clone())]);
            self.after_index_update(update, window, cx);
            self.ensure_open(&key.session, cx);
            if attempt.navigation == Some(self.navigation_revision)
                && self.composer_key.as_ref() == Some(&attempt.source)
                && self.drafts.get(&attempt.source).is_none()
            {
                self.select_session(key.session.clone(), window, cx);
            }
        }
        // Like the web's `finishDraftSession`, the mode write runs beside the
        // first send and never waits for or reorders it.
        self.write_created_permission_mode(
            key,
            attempt.request.directory(),
            client.clone(),
            &executor,
            (window, cx),
        );
        // This barrier snapshots creation removal and the session-owned attempt
        // together. A failed write retains the message without issuing a prompt.
        self.start_created_send(
            key.clone(),
            CapturedPrompt {
                prompt: dispatched,
                requests,
                admitted: Vec::new(),
            },
            &session,
            client,
            executor,
            (window, cx),
        );
    }

    pub(crate) fn mark_creation_deleted(&mut self, key: &SessionKey, cx: &mut Context<Self>) {
        self.permission_modes.creating.remove(key);
        let changed = self.creations.phase(key, CreationPhase::Deleted);
        let bound = self.send_attempts.mark_created_deleted(key);
        if changed || bound {
            self.start_background_save(cx);
            cx.notify();
        }
    }

    pub(crate) fn retry_created_prompt(
        &mut self,
        key: &SessionKey,
        requests: Vec<RequestKey>,
        admitted: Vec<MessageId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lookup_created_session(key, CreatedAction::Dispatch(requests, admitted), window, cx);
    }

    pub(crate) fn check_created_send(
        &mut self,
        key: &SessionKey,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let requests = self
            .send_attempts
            .created_attempt(key)
            .map_or_else(Vec::new, |attempt| {
                self.requests_for_creation(key, attempt.owner.directory())
            });
        self.lookup_created_session(
            key,
            if retry {
                CreatedAction::RetryPrompt(requests)
            } else {
                CreatedAction::ObservePrompt
            },
            window,
            cx,
        );
    }

    fn lookup_created_session(
        &mut self,
        key: &SessionKey,
        action: CreatedAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creation_tasks.contains_key(key)
            || key.runtime != self.runtime
            || !self.persistence_ready()
        {
            return;
        }
        let Some(attempt) = self.send_attempts.created_attempt(key).filter(|attempt| {
            attempt.origin == SendOrigin::Created && attempt.admission != Admission::Preparing
        }) else {
            return;
        };
        if self.send_attempts.is_active(key) {
            return;
        }
        let prompt = attempt.prompt.clone();
        let directory = attempt.owner.directory().to_owned();
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let executor = connection.network.handle();
        let captured = client.clone();
        let session = key.session.clone();
        let lookup = connection
            .network
            .run(async move { captured.lookup_session(&session, &directory).await });
        self.creations
            .checks
            .insert(key.clone(), CreationCheck::Checking);
        let key = key.clone();
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = lookup.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.creation_tasks.remove(&key);
                this.creations.checks.remove(&key);
                if !this
                    .send_attempts
                    .created_attempt(&key)
                    .is_some_and(|attempt| {
                        attempt.origin == SendOrigin::Created
                            && attempt.prompt.message_id == prompt.message_id
                    })
                {
                    return;
                }
                match result {
                    Some(Ok(SessionObservation::Found(session))) => {
                        if key.runtime == this.runtime {
                            let update = this
                                .sessions
                                .apply([SessionChange::Created((*session).clone())]);
                            this.after_index_update(update, window, cx);
                        }
                        match action {
                            CreatedAction::Dispatch(requests, admitted) => this
                                .dispatch_created_retry(
                                    key.clone(),
                                    CapturedPrompt {
                                        prompt: first_prompt_selection(&prompt, &session),
                                        requests,
                                        admitted,
                                    },
                                    &session,
                                    client,
                                    executor,
                                    (window, cx),
                                ),
                            CreatedAction::ObservePrompt => {
                                this.continue_created_recovery(&key, None, window, cx)
                            }
                            CreatedAction::RetryPrompt(requests) => {
                                this.continue_created_recovery(&key, Some(requests), window, cx)
                            }
                        }
                    }
                    Some(Ok(SessionObservation::NotObserved)) => {
                        this.mark_creation_deleted(&key, cx)
                    }
                    Some(Err(SessionError::IdentityMismatch | SessionError::LocationMismatch)) => {
                        this.creations
                            .checks
                            .insert(key.clone(), CreationCheck::Mismatch);
                    }
                    _ => {
                        this.creations
                            .checks
                            .insert(key.clone(), CreationCheck::Failed);
                    }
                }
                cx.notify();
            });
        });
        self.creation_tasks.insert(task_key, task);
        cx.notify();
    }

    pub(crate) fn creation_recovery_items(&self) -> Vec<RecoveryRow> {
        let connected = self
            .connection
            .as_ref()
            .is_some_and(|connection| matches!(connection.state, ConnectionState::Connected));
        self.creations
            .iter()
            .filter(|(key, _)| key.runtime == self.runtime)
            .map(|(key, attempt)| {
                let check = self.creations.checks.get(key);
                let phase = if check == Some(&CreationCheck::Checking) {
                    RecoveryPhase::Checking
                } else {
                    match attempt.phase {
                        CreationPhase::Preparing | CreationPhase::Confirming => {
                            RecoveryPhase::CreatePreparing
                        }
                        CreationPhase::Creating => RecoveryPhase::Creating,
                        CreationPhase::Deleted => RecoveryPhase::CreateDeleted,
                        CreationPhase::Mismatch => RecoveryPhase::CreateMismatch,
                        CreationPhase::Confirmed if check != Some(&CreationCheck::Failed) => {
                            RecoveryPhase::CreateReady
                        }
                        _ => match check {
                            Some(CreationCheck::Failed) => RecoveryPhase::CreateCheckFailed,
                            Some(CreationCheck::Mismatch) => RecoveryPhase::CreateMismatch,
                            Some(CreationCheck::NotObserved) => RecoveryPhase::CreateNotObserved,
                            _ => RecoveryPhase::CreateUnknown,
                        },
                    }
                };
                let available =
                    connected && !self.creation_tasks.contains_key(key) && self.persistence_ready();
                RecoveryRow {
                    key: key.clone(),
                    message: attempt.prompt.message_id.clone(),
                    session_label: attempt.source.directory().to_owned().into(),
                    text: attempt
                        .prompt
                        .text
                        .chars()
                        .take(240)
                        .collect::<String>()
                        .into(),
                    phase,
                    can_check: available
                        && !matches!(
                            attempt.phase,
                            CreationPhase::Preparing
                                | CreationPhase::Creating
                                | CreationPhase::Deleted
                        ),
                    can_restore: false,
                    can_retry: available
                        && self.persistence_ready()
                        && (attempt.phase == CreationPhase::Confirmed
                            || (attempt.phase == CreationPhase::Uncertain
                                && check == Some(&CreationCheck::NotObserved)
                                && self.creation_project_registered(attempt))),
                }
            })
            .collect()
    }
}
