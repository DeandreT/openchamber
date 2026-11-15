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

fn native_creation_identity_matches(attempt: &CreateAttempt, session: &SessionRecord) -> bool {
    if !attempt
        .native_config
        .as_ref()
        .is_some_and(|config| config.prepare_after_session)
    {
        return true;
    }
    attempt.request.agent().is_some()
        && session.agent.as_deref() == attempt.request.agent()
        && attempt
            .request
            .model()
            .zip(session.model.as_ref())
            .is_some_and(|(expected, actual)| {
                expected.provider_id == actual.provider_id && expected.model_id == actual.model_id
            })
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
        let Some(owner) = self.composer_key.as_ref() else {
            return false;
        };
        self.can_create_draft(owner)
            && self.send_selection().is_some_and(|(directory, selection)| {
                (self.chat_draft_open() || directory == owner.directory())
                    && selection.model.is_some()
            })
    }

    /// A retained activation keeps its selection while the composer accepts
    /// independent choices. Creation still requires the original draft on screen.
    pub(crate) fn can_create_starter(
        &self,
        capture: &crate::starter_capture::StarterCapture,
    ) -> bool {
        let owner = &capture.owner;
        self.can_create_draft(owner)
            && (if self.is_chat_creation(owner) {
                capture.project == openchamber_native_core::chat_draft::CHAT_PROJECT_ID
            } else {
                self.draft_destination_project(owner.directory())
                    .is_some_and(|project| project.id.as_str() == capture.project)
            })
            && capture.selection.model.as_ref().is_some_and(|model| {
                self.native_model_sendable(owner.runtime(), owner.directory(), None, model)
            })
    }

    fn can_create_draft(&self, owner: &DraftKey) -> bool {
        if !self.new_session_view_active()
            || self.composer_key.as_ref() != Some(owner)
            || !self.persistence_ready()
            || self.close_in_progress()
        {
            return false;
        }
        owner.runtime() == &self.runtime
            && matches!(owner.target(), DraftTarget::NewSession(_))
            && (if self.chat_draft_open() {
                self.chat_creation_ready(owner)
                    && self.chat_prepared_directory().as_deref() == Some(owner.directory())
            } else {
                self.draft_projects
                    .get(&self.runtime)
                    .is_some_and(|directory| directory == owner.directory())
                    && self.draft_destination_project(owner.directory()).is_some()
            })
            && self.creations.can_begin(owner)
            && self.connection.as_ref().is_some_and(|connection| {
                matches!(connection.state, ConnectionState::Connected)
                    && connection.client.is_some()
            })
    }

    pub(crate) fn submit_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_new_session_with(None, window, cx);
    }

    /// Creates the selected project's session and sends its first prompt:
    /// the composer's text, or a starter's rendered prompt. False when
    /// nothing was created.
    pub(crate) fn submit_new_session_with(
        &mut self,
        starter: Option<crate::starters::StarterPrompt>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if starter.is_none() && self.chat_draft_open() && self.chat_prepared_directory().is_none() {
            self.prepare_chat_draft(window, cx);
            return false;
        }
        let eligible = match starter.as_ref() {
            Some(starter) => self
                .starters
                .captures
                .get(&starter.capture_id)
                .is_some_and(|capture| self.can_create_starter(capture)),
            None => self.can_create_connected(),
        };
        if !eligible {
            return false;
        }
        let Some(source) = self.composer_key.clone() else {
            return false;
        };
        if starter.is_none() && !self.starters.captures.can_begin(&source) {
            return false;
        }
        let project = if self.is_chat_creation(&source) {
            openchamber_native_core::chat_draft::CHAT_PROJECT_ID.to_owned()
        } else {
            let Some(project) = self.draft_destination_project(source.directory()) else {
                return false;
            };
            project.id.to_string()
        };
        let from_starter = starter.is_some();
        let (
            draft,
            prompt_text,
            parts,
            selection,
            permission_mode,
            capture_id,
            effort,
            starter_native_config,
        ) = match starter {
            // A definite failure gives back what the chip submitted, as the
            // web restores `presetText`. All content and configuration belong
            // to the activation, independently of the now-current composer.
            Some(starter) => {
                let Some(capture) = self
                    .starters
                    .captures
                    .get(&starter.capture_id)
                    .filter(|capture| capture.owner == source && capture.project == project)
                else {
                    return false;
                };
                (
                    starter.draft,
                    starter.prompt_text,
                    starter.parts,
                    capture.selection.clone(),
                    capture.permission_mode,
                    Some(starter.capture_id),
                    starter.effort,
                    starter.native_config,
                )
            }
            None => {
                let Some((_, selection)) = self.send_selection() else {
                    return false;
                };
                let draft = self.composer_draft(cx);
                let text = draft.text.clone();
                let effort = selection
                    .model
                    .as_ref()
                    .map(|model| self.capture_user_effort(model));
                let Ok(parts) = self.capture_draft_parts(&source, &draft) else {
                    self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                    return false;
                };
                (draft, text, parts, selection, None, None, effort, None)
            }
        };
        if !draft.has_sendable_content() {
            return false;
        }
        let native_config = if from_starter {
            starter_native_config
        } else {
            selection
                .model
                .as_ref()
                .and_then(|model| self.capture_native_creation(model))
        };
        if native_config
            .as_ref()
            .is_some_and(|config| config.prepare_after_session)
            && selection.agent_name.is_none()
        {
            return false;
        }
        if selection.model.as_ref().is_some_and(|model| {
            self.native_owner_key(&model.provider_id)
                .is_some_and(|key| self.native_agents.owner.state(&key).is_some())
        }) && native_config
            .as_ref()
            .is_none_or(|capture| !capture.validate())
        {
            return false;
        }
        let id = self.session_ids.next();
        let Ok(request) = CreateSession::new(
            id.clone(),
            source.directory(),
            selection.model.clone(),
            selection.agent_name.clone(),
        ) else {
            return false;
        };
        // The goal's reminder ID is minted before the prompt's.
        let goal = self.take_created_goal(&prompt_text);
        let (message_id, created) = self.message_ids.next();
        let effort_id = message_id.clone();
        let key = SessionKey {
            runtime: self.runtime.clone(),
            session: id,
        };
        let native_creation = native_config.is_some();
        let attempt = CreateAttempt {
            source: source.clone(),
            project,
            request,
            draft: draft.clone(),
            resources: crate::attachment_preparation::prepared_resources(&draft),
            native_config,
            prompt: Prompt {
                message_id,
                text: prompt_text,
                parts,
                model: selection.model,
                agent: selection.agent_name,
                delivery: None,
            },
            created,
            phase: CreationPhase::Preparing,
            navigation: Some(self.navigation_revision),
        };
        if !self.creations.insert(key.clone(), attempt) {
            if goal.is_some() {
                self.goals.armed = true;
            }
            return false;
        }
        self.handoff_chat_directory(&source);
        if let Some(goal) = goal {
            self.goals.created.insert(effort_id.clone(), goal);
        }
        if let Some(effort) = effort {
            self.effort_preferences
                .retain_user_send(&key.runtime, &effort_id, effort);
        }
        // The draft's own permission choice goes with its session; the next
        // draft starts from the Settings default again.
        let permission_mode = if native_creation {
            None
        } else if from_starter {
            permission_mode
        } else {
            self.permission_modes.drafts.remove(&key.runtime)
        };
        if let Some(mode) = permission_mode {
            self.permission_modes.creating.insert(key.clone(), mode);
        }
        // A starter's later composer choices are already independent. The first
        // session opens from its captured prompt selection during handoff.
        if !from_starter && let Some(from) = self.selection_owner() {
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
        // A starter took its complete source when clicked. Transfer its resources
        // and rollback content to creation in this same foreground transition;
        // the preparation barrier persists both sides of that handoff together.
        if let Some(id) = capture_id {
            self.starters
                .captures
                .handoff(&id)
                .expect("the inserted creation takes its retained starter capture");
            self.save_composer_draft(cx);
        } else {
            self.drafts.take(&source);
            self.composer
                .update(cx, |composer, cx| composer.set_value("", window, cx));
            self.save_draft_preview(&source, &Draft::default(), cx);
        }
        let requests = self.requests_for_creation(&key, source.directory());
        self.start_creation_post(key, requests, window, cx);
        true
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
        let message_id = attempt.prompt.message_id.clone();
        if attempt.phase == CreationPhase::Uncertain {
            self.effort_preferences
                .forget_user_send(&key.runtime, &message_id);
        }
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
            self.effort_preferences
                .forget_user_send(&key.runtime, &message_id);
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
                        this.effort_preferences
                            .forget_user_send(&key.runtime, &message_id);
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
                        this.effort_preferences
                            .forget_user_send(&key.runtime, &message_id);
                        return;
                    }
                    if !matches!(&result, Some(Ok(_))) {
                        this.effort_preferences
                            .forget_user_send(&key.runtime, &message_id);
                    }
                    match result {
                        Some(Ok(session)) => {
                            this.creations.phase(&key, CreationPhase::Confirmed);
                            this.continue_created_session(
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
        if attempt.project == openchamber_native_core::chat_draft::CHAT_PROJECT_ID {
            return self.is_chat_creation(&attempt.source);
        }
        self.draft_destination_project(attempt.request.directory())
            .is_some_and(|project| project.id.as_str() == attempt.project)
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
        if retry {
            self.effort_preferences
                .forget_user_send(&key.runtime, &attempt.prompt.message_id);
        }
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
                            this.continue_created_session(
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

    /// The accepted first Send already owns a durable capture. Publish and save
    /// the real session before any session-first provider preparation. Restored
    /// attempts reach this method only through an explicit recovery retry.
    fn continue_created_session(
        &mut self,
        key: &SessionKey,
        session: SessionRecord,
        client: ServerClient,
        executor: tokio::runtime::Handle,
        requests: Vec<RequestKey>,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let (window, cx) = context;
        let Some(attempt) = self.creations.get(key).cloned() else {
            return;
        };
        let Some(config) = attempt
            .native_config
            .as_ref()
            .filter(|config| config.prepare_after_session)
        else {
            self.bind_created_session(key, session, client, executor, requests, (window, cx));
            return;
        };
        if !native_creation_identity_matches(&attempt, &session) {
            self.creations
                .checks
                .insert(key.clone(), CreationCheck::Mismatch);
            return;
        }
        if config.correction_required {
            if self.selected.as_ref() != Some(&key.session) {
                // Reveal the pending session's choices first. A retry on another
                // composer must not accept a configuration the user cannot see.
                self.select_session(key.session.clone(), window, cx);
                return;
            }
            if self
                .catalog_selection()
                .and_then(|(_, selection)| selection.model)
                .is_none_or(|model| model.provider_id != config.provider_id)
            {
                return;
            }
            let owner = openchamber_native_core::native_agent::OwnerKey {
                server: key.runtime.as_str().to_owned(),
                scope: openchamber_native_core::native_agent::Scope {
                    directory: attempt.source.directory().to_owned(),
                    provider_id: config.provider_id.clone(),
                    session_id: Some(key.session.to_string()),
                },
            };
            if !self.native_agents.owner.can_set_config(&owner) {
                // An explicit retry may first recover the current config-only
                // snapshot. It does not accept undisplayed corrections or send.
                self.bind_created_session(key, session, client, executor, requests, (window, cx));
                return;
            }
            let Some(state) = self.native_agents.owner.state(&owner) else {
                return;
            };
            let complete = state.config_complete;
            let choices = state
                .config
                .iter()
                .map(|field| (field.id.clone(), field.value.clone()))
                .collect();
            if !self.creations.correct_native_config(key, choices, complete) {
                return;
            }
        }
        let owner = openchamber_native_core::native_agent::OwnerKey {
            server: key.runtime.as_str().to_owned(),
            scope: openchamber_native_core::native_agent::Scope {
                directory: attempt.source.directory().to_owned(),
                provider_id: config.provider_id.clone(),
                session_id: Some(key.session.to_string()),
            },
        };
        let ready = self.native_agents.owner.can_set_config(&owner)
            && self.native_agents.owner.state(&owner).is_some_and(|state| {
                state.status == openchamber_native_core::native_agent::Status::Ready
                    && state.config_complete
                    && state.authentication
                        == openchamber_native_core::native_agent::Authentication::Authenticated
            });
        self.suspend_native_creation(&owner, ready);
        if key.runtime == self.runtime {
            self.save_composer_draft(cx);
            let update = self
                .sessions
                .apply([SessionChange::Created(session.clone())]);
            self.after_index_update(update, window, cx);
            if attempt.navigation == Some(self.navigation_revision)
                && self.composer_key.as_ref() == Some(&attempt.source)
                && self.drafts.get(&attempt.source).is_none()
            {
                self.select_session(key.session.clone(), window, cx);
            }
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
            let _ = this.update_in(cx, |this, window, cx| {
                this.creation_tasks.remove(&key);
                if this.record_persistence(stamp, &saved, cx) && this.persistence_ready() {
                    this.bind_created_session(
                        &key,
                        session,
                        client,
                        executor,
                        requests,
                        (window, cx),
                    );
                }
            });
        });
        self.creation_tasks.insert(task_key, task);
    }

    /// Native RPC/event continuations require a fresh real-session identity.
    /// This does not accept configuration corrections or replay a creation.
    pub(crate) fn resume_native_created_session(
        &mut self,
        key: &SessionKey,
        client: ServerClient,
        executor: tokio::runtime::Handle,
        requests: Vec<RequestKey>,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let (window, cx) = context;
        if self.creation_tasks.contains_key(key)
            || key.runtime != self.runtime
            || !self.persistence_ready()
        {
            return;
        }
        let Some(attempt) = self
            .creations
            .get(key)
            .filter(|attempt| attempt.phase == CreationPhase::Confirmed)
        else {
            return;
        };
        let directory = attempt.source.directory().to_owned();
        let session = key.session.clone();
        let captured = client.clone();
        let lookup = NetworkTask::spawn_on(&executor, async move {
            captured.lookup_session(&session, &directory).await
        });
        let key = key.clone();
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = lookup.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.creation_tasks.remove(&key);
                if this
                    .creations
                    .get(&key)
                    .is_none_or(|attempt| attempt.phase != CreationPhase::Confirmed)
                {
                    return;
                }
                match result {
                    Some(Ok(SessionObservation::Found(session))) => {
                        this.bind_created_session(
                            &key,
                            *session,
                            client,
                            executor,
                            requests,
                            (window, cx),
                        );
                    }
                    Some(Ok(SessionObservation::NotObserved)) => {
                        this.mark_creation_deleted(&key, cx)
                    }
                    _ => {
                        this.creations.checks.insert(key, CreationCheck::Failed);
                    }
                }
                cx.notify();
            });
        });
        self.creation_tasks.insert(task_key, task);
    }

    pub(crate) fn bind_created_session(
        &mut self,
        key: &SessionKey,
        session: SessionRecord,
        client: ServerClient,
        executor: tokio::runtime::Handle,
        requests: Vec<RequestKey>,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let (window, cx) = context;
        if !self.send_attempts.can_send(key) {
            return;
        }
        let Some(attempt) = self
            .creations
            .get(key)
            .filter(|attempt| attempt.phase == CreationPhase::Confirmed)
            .cloned()
        else {
            return;
        };
        if !native_creation_identity_matches(&attempt, &session) {
            self.creations
                .checks
                .insert(key.clone(), CreationCheck::Mismatch);
            return;
        }
        if !self.prepare_native_created_session(key, &client, &executor, &requests, (window, cx)) {
            return;
        }
        let Some(attempt) = self.creations.get(key).cloned() else {
            return;
        };
        if !self.send_attempts.can_send(key) {
            self.effort_preferences
                .forget_user_send(&key.runtime, &attempt.prompt.message_id);
            return;
        }
        let Some(owner) = DraftKey::new(
            key.runtime.clone(),
            attempt.request.directory(),
            DraftTarget::Session(key.session.clone()),
        ) else {
            self.effort_preferences
                .forget_user_send(&key.runtime, &attempt.prompt.message_id);
            return;
        };
        let dispatched = first_prompt_selection(&attempt.prompt, &session);
        let selection_owner = openchamber_native_core::composer_selection::SelectionOwner {
            draft: owner.clone(),
            project: (attempt.project != openchamber_native_core::chat_draft::CHAT_PROJECT_ID)
                .then(|| openchamber_native_core::ids::ProjectId::from(attempt.project.clone())),
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
                synthetic: None,
            },
            parts: vec![Part::text(
                PartId::new(format!("{}-local-text", attempt.prompt.message_id)),
                attempt.prompt.text.clone(),
            )],
        };
        let native_creation = attempt.native_config.is_some();
        let send = SendAttempt {
            origin: SendOrigin::Created,
            owner,
            resources: attempt.resources,
            draft: attempt.draft,
            prompt: attempt.prompt,
            optimistic,
            admission: Admission::Preparing,
            supersedes: true,
            queue_owned: false,
            reservation: None,
        };
        if !self.send_attempts.begin(key.clone(), send) {
            self.effort_preferences
                .forget_user_send(&key.runtime, &dispatched.message_id);
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
            if attempt.project == openchamber_native_core::chat_draft::CHAT_PROJECT_ID {
                self.reveal_created_chat_section(window, cx);
            }
        }
        self.renew_chat_folder(&key.runtime, &attempt.source, window, cx);
        // Like the web's `finishDraftSession`, the mode write runs beside the
        // first send and never waits for or reorders it.
        if !native_creation {
            self.write_created_permission_mode(
                key,
                attempt.request.directory(),
                client.clone(),
                &executor,
                (window, cx),
            );
        }
        // This barrier snapshots creation removal and the session-owned attempt
        // together. A failed write retains the message without issuing a prompt.
        self.start_created_send(
            key.clone(),
            CapturedPrompt {
                prompt: dispatched,
                requests,
                admitted: Vec::new(),
                consumes_composer: false,
                supersedes: true,
            },
            &session,
            client,
            executor,
            (window, cx),
        );
    }

    /// Use the retained settings writer after this workspace mutation releases
    /// its borrow. The reveal belongs only to the runtime that created the chat.
    fn reveal_created_chat_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .send_catalog
            .settings
            .as_ref()
            .is_none_or(|settings| settings.show_chats != Some(false))
        {
            return;
        }
        let settings = self.ensure_settings_model(window, cx);
        let general = settings.read(cx).general.clone();
        let owner = cx.weak_entity();
        let runtime = self.runtime.clone();
        cx.defer(move |cx| {
            let Some(owner) = owner.upgrade() else {
                return;
            };
            if owner.read(cx).runtime != runtime {
                return;
            }
            general.update(cx, |general, cx| {
                general.refresh(cx);
                general.change(
                    crate::general_settings::SHOW_CHATS,
                    crate::general_settings::Choice::Boolean(true),
                    cx,
                );
            });
        });
    }

    pub(crate) fn mark_creation_deleted(&mut self, key: &SessionKey, cx: &mut Context<Self>) {
        if let Some(id) = self
            .creations
            .get(key)
            .map(|attempt| attempt.prompt.message_id.clone())
            .or_else(|| {
                self.send_attempts
                    .created_attempt(key)
                    .map(|attempt| attempt.prompt.message_id.clone())
            })
        {
            self.effort_preferences.forget_user_send(&key.runtime, &id);
        }
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
                                        consumes_composer: false,
                                        supersedes: true,
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
