//! Window-owned native-agent tasks. Protocol authority and validation stay in core.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{AppContext, Context, Task, Window};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::native_agent::{
    AgentEvent, Call, Config, ConfigChange, ConfigRole, NativeAgents, OwnerKey, PermissionOwner,
    Reply, Scope, Status, ToolIdentity, tool_authority,
};
use openchamber_native_core::send_config::{ModelChoice, Selection};
use openchamber_native_core::transcript::{PartContent, TranscriptEvent};
use openchamber_native_transport::native_agent::{NativeAgentApi, NativeAgentProvider};

use crate::connection::ConnectionState;
use crate::workspace::Workspace;

#[derive(Clone)]
struct CreatedContinuation {
    window: gpui_kit::AnyWindowHandle,
    key: crate::send_attempt::SessionKey,
    client: openchamber_native_transport::client::ServerClient,
    executor: tokio::runtime::Handle,
    requests: Vec<openchamber_native_core::pending_requests::RequestKey>,
}

/// Credential-free preference custody for the first-session handoff. The plugin
/// revalidates these values through setConfig and publishes a new session binding.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreationConfig {
    pub provider_id: String,
    pub model_id: String,
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prepare_after_session: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub correction_required: bool,
    pub config: Vec<(String, String)>,
}

pub(crate) const CONFIG_CATALOG_LIMIT: usize = 32;
// Retain one maximum captured catalog plus one maximum correction catalog.
// Repeated partial discoveries cannot grow the persisted intent indefinitely.
pub(crate) const PENDING_CONFIG_LIMIT: usize = CONFIG_CATALOG_LIMIT * 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigPick {
    pub owner: OwnerKey,
    pub activation_id: String,
    pub revision: u64,
    pub field_id: String,
    pub choice_id: String,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn creation_correction(
    ticket: &openchamber_native_core::native_agent::Ticket,
    reply: &Reply,
) -> bool {
    let Reply::Failure { code, outcome, .. } = reply else {
        return false;
    };
    if *code != openchamber_native_core::native_agent::ErrorCode::InvalidConfig
        && !(*outcome == openchamber_native_core::native_agent::Outcome::Unknown
            && matches!(ticket.call, Call::SetConfig(_)))
    {
        return false;
    }
    true
}

impl CreationConfig {
    /// Before allocation only known fields are applicable. Unknown IDs remain
    /// captured and must all validate after prepare discovers the live catalog.
    fn changes(
        &self,
        state: &openchamber_native_core::native_agent::State,
        complete: bool,
    ) -> Result<Vec<ConfigChange>, ()> {
        if complete && self.prepare_after_session && !state.config_complete {
            return Err(());
        }
        let mut changes = Vec::new();
        for (id, value) in &self.config {
            let Some(field) = state.config.iter().find(|field| field.id == *id) else {
                if complete {
                    return Err(());
                }
                continue;
            };
            if !field.choices.iter().any(|choice| choice.id == *value) {
                return Err(());
            }
            changes.push(ConfigChange {
                id: id.clone(),
                value: value.clone(),
            });
        }
        Ok(changes)
    }
    /// These are advertised choice IDs, never editable credential material.
    /// The plugin remains responsible for keeping secrets out of its catalog.
    pub(crate) fn validate(&self) -> bool {
        use openchamber_native_core::native_agent::valid_id;
        let mut ids = BTreeSet::new();
        valid_id(&self.provider_id)
            && valid_id(&self.model_id)
            && (if self.prepare_after_session {
                self.variant.is_none()
            } else {
                self.variant.as_deref().is_some_and(valid_id) && !self.correction_required
            })
            && self.config.len()
                <= if self.prepare_after_session {
                    PENDING_CONFIG_LIMIT
                } else {
                    CONFIG_CATALOG_LIMIT
                }
            && self.config.iter().all(|(id, value)| {
                let normalized: String = id
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect();
                valid_id(id)
                    && valid_id(value)
                    && ids.insert(id)
                    && !matches!(
                        normalized.as_str(),
                        "apikey"
                            | "token"
                            | "password"
                            | "secret"
                            | "authorization"
                            | "credentials"
                            | "accesstoken"
                            | "refreshtoken"
                            | "nativeconfigtoken"
                    )
            })
    }
}

#[derive(Default)]
pub(crate) struct NativeAgentTasks {
    pub owner: NativeAgents,
    prepared: BTreeSet<OwnerKey>,
    reads: BTreeMap<OwnerKey, (String, Task<()>)>,
    mutations: BTreeMap<OwnerKey, (String, Task<()>)>,
    creations: BTreeMap<OwnerKey, (String, Task<()>)>,
    creation_bound: BTreeSet<OwnerKey>,
    creation_preconfigured: BTreeSet<OwnerKey>,
    creation_prepared: BTreeSet<OwnerKey>,
    waiting: BTreeMap<OwnerKey, CreatedContinuation>,
    providers: BTreeMap<OwnerKey, NativeAgentProvider>,
    /// Retained immutable preference snapshots keyed by their published routing binding.
    captures: BTreeMap<(OwnerKey, String, String), CreationConfig>,
    replay_through: Option<String>,
}

impl NativeAgentTasks {
    fn remember(&mut self, key: &OwnerKey) {
        let Some(state) = self.owner.state(key) else {
            return;
        };
        let Some(execution) = &state.execution else {
            return;
        };
        self.captures.insert(
            (
                key.clone(),
                execution.model_id.clone(),
                execution.variant.clone(),
            ),
            CreationConfig {
                provider_id: execution.provider_id.clone(),
                model_id: execution.model_id.clone(),
                variant: Some(execution.variant.clone()),
                prepare_after_session: false,
                correction_required: false,
                config: state
                    .config
                    .iter()
                    .map(|field| (field.id.clone(), field.value.clone()))
                    .collect(),
            },
        );
    }

    pub(crate) fn connected(&mut self, resumed: bool, through: Option<String>) {
        self.invalidate();
        self.replay_through = resumed.then_some(through).flatten();
    }

    pub(crate) fn invalidate(&mut self) {
        self.owner.invalidate();
        self.prepared.clear();
        self.reads.clear();
        self.mutations.clear();
        self.creations.clear();
        self.creation_bound.clear();
        self.creation_preconfigured.clear();
        self.creation_prepared.clear();
    }

    pub(crate) fn replaying(&self) -> bool {
        self.replay_through.is_some()
    }

    pub(crate) fn frame_applied(&mut self, event_id: Option<&str>) {
        if event_id.is_some() && self.replay_through.as_deref() == event_id {
            self.replay_through = None;
        }
    }
}

/// Replayed calls are history, including their executed bit. Demote provenance
/// before the transcript sees the event, so a later progress frame cannot use it.
pub(crate) fn demote_replayed_execution(event: &mut TranscriptEvent) {
    let facts = match event {
        TranscriptEvent::PartUpdated { part, .. } => match &mut part.content {
            PartContent::Tool { facts, .. } => Some(facts.as_mut()),
            _ => None,
        },
        TranscriptEvent::ToolTransition { facts, .. } => facts.as_deref_mut(),
        _ => None,
    };
    if let Some(execution) = facts.and_then(|facts| facts.execution.as_mut()) {
        execution.live = false;
    }
}

impl Workspace {
    fn record_creation_correction(
        &mut self,
        ticket: &openchamber_native_core::native_agent::Ticket,
        correction: bool,
        cx: &mut Context<Self>,
    ) {
        if !correction {
            return;
        }
        let Some(session) = ticket.key.scope.session_id.as_deref() else {
            return;
        };
        let key = crate::send_attempt::SessionKey {
            runtime: RuntimeId::from(ticket.key.server.as_str()),
            session: SessionId::from(session),
        };
        if self.creations.require_native_correction(&key) {
            self.start_background_save(cx);
        }
    }

    pub(crate) fn suspend_native_creation(&mut self, owner: &OwnerKey, ready: bool) {
        // Events and login replies cannot resume corrected intent while its
        // acceptance is still waiting for the journal barrier.
        self.native_agents.waiting.remove(owner);
        self.native_agents.creation_bound.remove(owner);
        if ready {
            self.native_agents
                .creation_preconfigured
                .insert(owner.clone());
            self.native_agents.creation_prepared.insert(owner.clone());
        } else {
            self.native_agents.creation_preconfigured.remove(owner);
            self.native_agents.creation_prepared.remove(owner);
        }
    }

    pub(crate) fn native_action_pending(&self, key: &OwnerKey) -> bool {
        self.connection
            .as_ref()
            .is_none_or(|connection| connection.state != ConnectionState::Connected)
            || self.native_agents.replaying()
            || self.native_agents.reads.contains_key(key)
            || self.native_agents.mutations.contains_key(key)
            || self.native_agents.creations.contains_key(key)
    }
    pub(crate) fn native_config_fields(&self, role: ConfigRole) -> Vec<&Config> {
        let Some(model) = self
            .catalog_selection()
            .and_then(|(_, selection)| selection.model)
        else {
            return Vec::new();
        };
        let Some(key) = self.native_owner_key(&model.provider_id) else {
            return Vec::new();
        };
        self.native_agents
            .owner
            .state(&key)
            .map(|state| {
                state
                    .config
                    .iter()
                    .filter(|field| field.role == role)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn native_role_label(&self, role: ConfigRole) -> Option<String> {
        let labels: Vec<_> = self
            .native_config_fields(role)
            .into_iter()
            .filter_map(|field| {
                field
                    .choices
                    .iter()
                    .find(|choice| choice.id == field.value)
                    .map(|choice| choice.label.as_str())
            })
            .collect();
        (!labels.is_empty()).then(|| labels.join(" / "))
    }

    pub(crate) fn choose_native_config(
        &mut self,
        pick: &ConfigPick,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.composer_editable()
            || self
                .native_owner_key(&pick.owner.scope.provider_id)
                .as_ref()
                != Some(&pick.owner)
        {
            return false;
        }
        if !self.native_agents.owner.can_set_config(&pick.owner)
            || self.native_action_pending(&pick.owner)
        {
            return false;
        }
        let Some(state) = self.native_agents.owner.state(&pick.owner) else {
            return false;
        };
        if state.activation_id != pick.activation_id
            || state.revision != pick.revision
            || !state.capabilities.set_config
        {
            return false;
        }
        if let Some(session) = &pick.owner.scope.session_id {
            let pending = crate::send_attempt::SessionKey {
                runtime: RuntimeId::from(pick.owner.server.as_str()),
                session: SessionId::from(session.as_str()),
            };
            if self.creations.require_native_correction(&pending) {
                self.start_background_save(cx);
            }
        }
        self.native_call(
            pick.owner.clone(),
            Call::SetConfig(vec![ConfigChange {
                id: pick.field_id.clone(),
                value: pick.choice_id.clone(),
            }]),
            false,
            cx,
        )
    }

    pub(crate) fn native_config_options(
        &self,
        role: ConfigRole,
    ) -> Vec<(ConfigPick, String, bool)> {
        let Some(model) = self
            .catalog_selection()
            .and_then(|(_, selection)| selection.model)
        else {
            return Vec::new();
        };
        let Some(key) = self.native_owner_key(&model.provider_id) else {
            return Vec::new();
        };
        let Some(state) = self.native_agents.owner.state(&key) else {
            return Vec::new();
        };
        let fields: Vec<_> = state
            .config
            .iter()
            .filter(|field| field.role == role)
            .collect();
        let grouped = fields.len() > 1;
        fields
            .into_iter()
            .flat_map(|field| {
                field
                    .choices
                    .iter()
                    .map(|choice| {
                        (
                            ConfigPick {
                                owner: key.clone(),
                                activation_id: state.activation_id.clone(),
                                revision: state.revision,
                                field_id: field.id.clone(),
                                choice_id: choice.id.clone(),
                            },
                            if grouped {
                                format!("{}: {}", field.label, choice.label)
                            } else {
                                choice.label.clone()
                            },
                            choice.id == field.value,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub(crate) fn native_provider_advertised(&self, provider_id: &str) -> bool {
        self.native_provider(provider_id).is_some()
    }

    fn native_provider(&self, provider_id: &str) -> Option<NativeAgentProvider> {
        self.config_directory()
            .as_deref()
            .and_then(|directory| self.send_catalog.directory(directory))?
            .native_agents
            .iter()
            .find(|provider| provider.provider_id() == provider_id)
            .cloned()
    }

    pub(crate) fn native_owner_key(&self, provider_id: &str) -> Option<OwnerKey> {
        let draft = self.composer_key.as_ref()?;
        if draft.runtime() != &self.runtime {
            return None;
        }
        // Chat roots are configuration scope only. No RPC starts a location
        // until the real execution folder is ready.
        let directory = if self.selected.is_none() && self.chat_draft_open() {
            self.chat_prepared_directory()?
        } else {
            draft.directory().to_owned()
        };
        Some(OwnerKey {
            server: self.runtime.as_str().to_owned(),
            scope: Scope {
                directory,
                provider_id: provider_id.to_owned(),
                session_id: self.selected.as_ref().map(ToString::to_string),
            },
        })
    }

    /// One preparation per selected owner and connection, never authentication.
    pub(crate) fn ensure_native_agent(&mut self, cx: &mut Context<Self>) {
        if !self.composer_editable() || self.native_agents.replaying() {
            return;
        }
        if self.selected.as_ref().is_some_and(|session| {
            self.creations
                .get(&crate::send_attempt::SessionKey {
                    runtime: self.runtime.clone(),
                    session: session.clone(),
                })
                .and_then(|attempt| attempt.native_config.as_ref())
                .is_some_and(|config| config.prepare_after_session)
        }) {
            return;
        }
        let Some((_, selection)) = self.catalog_selection() else {
            return;
        };
        let Some(model) = selection.model else {
            return;
        };
        if self.native_provider(&model.provider_id).is_none() {
            return;
        }
        let Some(key) = self.native_owner_key(&model.provider_id) else {
            return;
        };
        if !self.native_agents.prepared.contains(&key)
            && !self.native_agents.reads.contains_key(&key)
        {
            self.native_call(key, Call::Prepare, true, cx);
        }
    }

    /// Reconnect reads only scopes already retained for visible or pending work.
    pub(crate) fn refresh_native_owners(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<_> = self
            .native_agents
            .owner
            .states()
            .filter(|(key, state)| {
                key.server == self.runtime.as_str() && !state.requests.is_empty()
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.native_call(key, Call::GetState, true, cx);
        }
        self.ensure_native_agent(cx);
    }

    pub(crate) fn native_call(
        &mut self,
        key: OwnerKey,
        call: Call,
        discover: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if key.server != self.runtime.as_str() || self.native_agents.replaying() {
            return false;
        }
        let call = if matches!(call, Call::Prepare)
            && !self.native_agents.creation_preconfigured.contains(&key)
            && key.scope.session_id.as_ref().is_some_and(|session| {
                self.creations
                    .get(&crate::send_attempt::SessionKey {
                        runtime: self.runtime.clone(),
                        session: SessionId::from(session.as_str()),
                    })
                    .and_then(|attempt| attempt.native_config.as_ref())
                    .is_some_and(|config| config.prepare_after_session)
            }) {
            // A status check may inspect the pending session before its frozen
            // preconfiguration is applied, but cannot allocate the wrong runtime.
            Call::GetState
        } else {
            call
        };
        if matches!(call, Call::Authenticate) && self.enterprise.enterprise_mode() {
            return false;
        }
        let mutation = call.is_mutation();
        if (mutation && self.native_agents.mutations.contains_key(&key))
            || (!mutation && self.native_agents.reads.contains_key(&key))
        {
            return false;
        }
        let Some(provider) = self
            .native_agents
            .providers
            .get(&key)
            .cloned()
            .or_else(|| self.native_provider(&key.scope.provider_id))
        else {
            return false;
        };
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return false;
        };
        let Some(client) = connection.client.clone() else {
            return false;
        };
        let Ok(api) = NativeAgentApi::new(client, key.clone(), &provider) else {
            return false;
        };
        let (id, _) = self.message_ids.next();
        let request_id = format!("native-{id}");
        let Ok(ticket) = self
            .native_agents
            .owner
            .begin(key.clone(), request_id, call, discover)
        else {
            return false;
        };
        self.native_agents.providers.insert(key.clone(), provider);
        if matches!(ticket.call, Call::Prepare) {
            self.native_agents.prepared.insert(key.clone());
        }
        let task = connection.network.run({
            let ticket = ticket.clone();
            async move { api.call(&ticket).await }
        });
        let stored_key = key.clone();
        let stored_request_id = ticket.request_id.clone();
        let read = cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let current = if mutation {
                    this.native_agents.mutations.get(&key)
                } else {
                    this.native_agents.reads.get(&key)
                };
                if current.is_none_or(|(id, _)| id != &ticket.request_id) {
                    return;
                }
                if mutation {
                    this.native_agents.mutations.remove(&key);
                } else {
                    this.native_agents.reads.remove(&key);
                }
                this.finish_native_call(&ticket, result, cx);
            });
        });
        if mutation {
            self.native_agents
                .mutations
                .insert(stored_key, (stored_request_id, read));
        } else {
            self.native_agents
                .reads
                .insert(stored_key, (stored_request_id, read));
        }
        true
    }

    /// Reconcile the response before deciding whether preparation failed. A newer
    /// live state can supersede the successful response's older snapshot.
    pub(crate) fn finish_native_call(
        &mut self,
        ticket: &openchamber_native_core::native_agent::Ticket,
        result: Option<Result<Reply, openchamber_native_transport::error::TransportError>>,
        cx: &mut Context<Self>,
    ) {
        let resume = matches!(&result, Some(Ok(Reply::Success { .. })));
        let failed = match result {
            Some(Ok(reply)) => {
                let correction = creation_correction(ticket, &reply);
                let outcome = match &reply {
                    Reply::Success { .. } => Ok(()),
                    Reply::Failure { code, .. } => Err(*code),
                };
                if self.native_agents.owner.complete(ticket, reply) {
                    self.record_creation_correction(ticket, correction, cx);
                    if matches!(ticket.call, Call::Authenticate)
                        && self
                            .native_agents
                            .owner
                            .state(&ticket.key)
                            .is_some_and(|state| {
                                state.capabilities.prepare_after_session
                                    && (state.status != Status::Ready || !state.config_complete)
                            })
                    {
                        // Explicit login may succeed before SDK preparation.
                        self.native_agents.creation_prepared.remove(&ticket.key);
                    }
                    let failed = matches!(ticket.call, Call::Prepare)
                        && crate::starters::native_preparation_failed(
                            Some(outcome),
                            self.native_agents.owner.state(&ticket.key),
                        );
                    self.native_agents.remember(&ticket.key);
                    if !failed {
                        self.retain_starter_native_ready(&ticket.key, cx);
                    }
                    self.native_requests.dirty = true;
                    failed
                } else {
                    true
                }
            }
            Some(Err(_)) | None => {
                self.native_agents.owner.failed(ticket);
                true
            }
        };
        if matches!(ticket.call, Call::Prepare) && failed {
            self.fail_starter_native_preparation(&ticket.key, cx);
        }
        if resume
            && !failed
            && let Some(next) = self.native_agents.waiting.get(&ticket.key).cloned()
        {
            let entity = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = cx.update_window(next.window, |_, window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.resume_native_created_session(
                            &next.key,
                            next.client,
                            next.executor,
                            next.requests,
                            (window, cx),
                        );
                    });
                });
            });
        }
        cx.notify();
    }

    pub(crate) fn native_send_selection(&self, selection: &Selection) -> Option<Selection> {
        let Some(model) = selection.model.as_ref() else {
            return Some(selection.clone());
        };
        if self.native_provider(&model.provider_id).is_none() {
            return Some(selection.clone());
        }
        let key = self.native_owner_key(&model.provider_id)?;
        if self.native_agents.owner.can_prepare_after_session(&key) {
            return selection.agent_name.as_ref().map(|_| selection.clone());
        }
        if !self.native_agents.owner.can_submit(&key)
            || self.native_agents.mutations.contains_key(&key)
        {
            return None;
        }
        let execution = self.native_agents.owner.state(&key)?.execution.as_ref()?;
        Some(Selection {
            model: Some(execution.model_choice()),
            agent_name: selection.agent_name.clone(),
        })
    }

    /// A captured variant can differ from today's selection. The provider owns
    /// its immutable registry and rejects unknown variants without fallback.
    pub(crate) fn native_model_sendable(
        &self,
        runtime: &RuntimeId,
        directory: &str,
        session: Option<&SessionId>,
        model: &ModelChoice,
    ) -> bool {
        let key = OwnerKey {
            server: runtime.as_str().to_owned(),
            scope: Scope {
                directory: directory.to_owned(),
                provider_id: model.provider_id.clone(),
                session_id: session.map(ToString::to_string),
            },
        };
        let known_native = self.native_agents.owner.state(&key).is_some()
            || (runtime == &self.runtime && self.native_provider(&model.provider_id).is_some());
        !known_native
            || self.native_agents.owner.can_prepare_after_session(&key)
            || (model
                .variant
                .as_ref()
                .is_some_and(|variant| !variant.is_empty())
                && self.native_agents.owner.can_submit(&key)
                && !self.native_agents.mutations.contains_key(&key))
    }

    pub(crate) fn capture_native_creation(&self, model: &ModelChoice) -> Option<CreationConfig> {
        let key = self.native_owner_key(&model.provider_id)?;
        if self.native_agents.owner.can_prepare_after_session(&key) {
            let state = self.native_agents.owner.state(&key)?;
            return Some(CreationConfig {
                provider_id: model.provider_id.clone(),
                model_id: model.model_id.clone(),
                variant: None,
                prepare_after_session: true,
                correction_required: false,
                config: state
                    .config
                    .iter()
                    .map(|field| (field.id.clone(), field.value.clone()))
                    .collect(),
            });
        }
        self.native_agents
            .captures
            .get(&(key, model.model_id.clone(), model.variant.clone()?))
            .cloned()
    }

    pub(crate) fn native_queued_send_ready(
        &mut self,
        configuration: &openchamber_native_core::message_queue::QueuedSendConfig,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.native_provider(&configuration.provider_id).is_none() {
            return true;
        }
        let Some(key) = self.native_owner_key(&configuration.provider_id) else {
            return false;
        };
        let model = ModelChoice {
            provider_id: configuration.provider_id.clone(),
            model_id: configuration.model_id.clone(),
            variant: configuration.variant.clone(),
        };
        let ready = self.native_model_sendable(
            &self.runtime,
            &key.scope.directory,
            self.selected.as_ref(),
            &model,
        );
        if !ready && !self.native_agents.prepared.contains(&key) {
            self.native_call(key, Call::Prepare, true, cx);
        }
        ready
    }

    pub(crate) fn native_permission_owner(&self) -> Option<PermissionOwner> {
        let model = self.catalog_selection()?.1.model?;
        let key = self.native_owner_key(&model.provider_id)?;
        self.native_agents
            .owner
            .state(&key)
            .map(|state| state.permission_owner)
    }

    pub(crate) fn native_event(
        &mut self,
        event: AgentEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.native_agents.replaying() {
            return;
        }
        let mut resume = None;
        let changed = match event {
            AgentEvent::StateChanged(state) => {
                let preparation_failed = matches!(state.status, Status::Error | Status::Stopped);
                let key = OwnerKey {
                    server: self.runtime.as_str().to_owned(),
                    scope: state.scope.clone(),
                };
                let changed = self
                    .native_agents
                    .owner
                    .state_changed(self.runtime.as_str(), state);
                if changed {
                    self.native_agents.remember(&key);
                    self.retain_starter_native_ready(&key, cx);
                    if preparation_failed {
                        self.fail_starter_native_preparation(&key, cx);
                    }
                    self.native_requests.dirty = true;
                    resume = self.native_agents.waiting.get(&key).cloned();
                }
                changed
            }
            AgentEvent::ToolProgress(progress) => {
                let Some(session) = progress.scope.session_id.as_ref() else {
                    return;
                };
                let Some(open) = self.open_sessions.get(&SessionId::from(session.as_str())) else {
                    return;
                };
                let tool = progress.tool.clone();
                let Some(authority) = tool_authority(&open.transcript, &tool) else {
                    return;
                };
                let key = OwnerKey {
                    server: self.runtime.as_str().to_owned(),
                    scope: progress.scope.clone(),
                };
                self.native_agents
                    .owner
                    .tool_progress(&key, progress, &authority)
            }
        };
        if changed {
            if let Some(next) = resume {
                self.resume_native_created_session(
                    &next.key,
                    next.client,
                    next.executor,
                    next.requests,
                    (window, cx),
                );
            }
            cx.notify();
        }
    }

    pub(crate) fn native_progress(&self, session: &SessionId, tool: &ToolIdentity) -> Option<&str> {
        let open = self.open_sessions.get(session)?;
        let authority = tool_authority(&open.transcript, tool)?;
        let record = self.sessions.get(session)?;
        let key = OwnerKey {
            server: self.runtime.as_str().to_owned(),
            scope: Scope {
                directory: record.resolved_directory()?,
                provider_id: authority.provider_id.to_owned(),
                session_id: Some(session.to_string()),
            },
        };
        self.native_agents.owner.progress_text(&key, &authority)
    }

    /// Gate the durable creation handoff before any first-prompt admission.
    pub(crate) fn prepare_native_created_session(
        &mut self,
        key: &crate::send_attempt::SessionKey,
        client: &openchamber_native_transport::client::ServerClient,
        executor: &tokio::runtime::Handle,
        requests: &[openchamber_native_core::pending_requests::RequestKey],
        context: (&mut Window, &mut Context<Self>),
    ) -> bool {
        let (window, cx) = context;
        if self.native_agents.replaying() {
            return false;
        }
        let Some(attempt) = self.creations.get(key) else {
            return false;
        };
        let Some(capture) = attempt.native_config.clone() else {
            return true;
        };
        let owner = OwnerKey {
            server: key.runtime.as_str().to_owned(),
            scope: Scope {
                directory: attempt.source.directory().to_owned(),
                provider_id: capture.provider_id.clone(),
                session_id: Some(key.session.to_string()),
            },
        };
        if capture.correction_required && self.native_agents.owner.can_set_config(&owner) {
            return false;
        }
        if let Some(state) = self.native_agents.owner.state(&owner) {
            let matches = (!capture.prepare_after_session
                || self.native_agents.creation_prepared.contains(&owner))
                && (capture.prepare_after_session || capture.config.len() == state.config.len())
                && capture.changes(state, true).is_ok()
                && capture.config.iter().all(|(id, value)| {
                    state
                        .config
                        .iter()
                        .any(|field| field.id == *id && field.value == *value)
                });
            if matches
                && (capture.config.is_empty() || self.native_agents.creation_bound.contains(&owner))
                && self.native_agents.owner.can_submit(&owner)
                && let Some(execution) = &state.execution
            {
                let model = execution.model_choice();
                let bound = self.creations.bind_native_execution(key, model);
                if bound {
                    self.native_agents.waiting.remove(&owner);
                    self.native_agents.creation_bound.remove(&owner);
                    self.native_agents.creation_preconfigured.remove(&owner);
                    self.native_agents.creation_prepared.remove(&owner);
                }
                return bound;
            }
        }
        if capture.prepare_after_session && self.native_agents.creation_bound.contains(&owner) {
            // A successful config mutation may precede variant publication.
            // Wait for state instead of repeating the mutation automatically.
            return false;
        }
        self.native_agents.waiting.insert(
            owner.clone(),
            CreatedContinuation {
                window: window.window_handle(),
                key: key.clone(),
                client: client.clone(),
                executor: executor.clone(),
                requests: requests.to_vec(),
            },
        );
        if self.native_agents.creations.contains_key(&owner)
            || (capture.prepare_after_session
                && (self.native_agents.reads.contains_key(&owner)
                    || self.native_agents.mutations.contains_key(&owner)))
        {
            return false;
        }
        let call = if capture.correction_required
            || (capture.prepare_after_session
                && self.native_agents.owner.requires_discovery(&owner))
        {
            Call::GetState
        } else {
            match self.native_agents.owner.state(&owner) {
                None if capture.prepare_after_session => Call::GetState,
                None => Call::Prepare,
                Some(state)
                    if capture.prepare_after_session
                        && !self.native_agents.creation_preconfigured.contains(&owner) =>
                {
                    let Ok(changes) = capture.changes(state, false) else {
                        self.creations.require_native_correction(key);
                        self.start_background_save(cx);
                        return false;
                    };
                    if changes.is_empty() {
                        self.native_agents
                            .creation_preconfigured
                            .insert(owner.clone());
                        Call::Prepare
                    } else {
                        Call::SetConfig(changes)
                    }
                }
                Some(_)
                    if capture.prepare_after_session
                        && !self.native_agents.creation_prepared.contains(&owner) =>
                {
                    Call::Prepare
                }
                Some(state)
                    if state.status == Status::Ready
                        && (capture.prepare_after_session
                            || self.native_agents.owner.can_submit(&owner))
                        && (!capture.prepare_after_session || state.config_complete)
                        && state.authentication
                            == openchamber_native_core::native_agent::Authentication::Authenticated
                        && matches!(
                            state.submission,
                            openchamber_native_core::native_agent::Submission::None
                        )
                        && self.native_agents.owner.can_set_config(&owner) =>
                {
                    let Ok(changes) = capture.changes(state, true) else {
                        if self.creations.require_native_correction(key) {
                            self.start_background_save(cx);
                        }
                        return false;
                    };
                    if changes.is_empty() {
                        return false;
                    }
                    Call::SetConfig(changes)
                }
                // Waiting authentication, uncertain mutations and stopped runtimes
                // cannot turn creation recovery into an implicit replay.
                Some(_) => return false,
            }
        };
        let provider = self
            .native_agents
            .providers
            .iter()
            .find(|(key, _)| {
                key.server == owner.server
                    && key.scope.provider_id == owner.scope.provider_id
                    && key.scope.directory == owner.scope.directory
            })
            .map(|(_, value)| value.clone())
            .or_else(|| self.native_provider(&owner.scope.provider_id));
        let Some(provider) = provider else {
            return false;
        };
        let Ok(api) = NativeAgentApi::new(client.clone(), owner.clone(), &provider) else {
            return false;
        };
        let (id, _) = self.message_ids.next();
        let request_id = format!("native-create-{id}");
        let discover = matches!(call, Call::Prepare | Call::GetState);
        let preconfigure = capture.prepare_after_session
            && matches!(call, Call::SetConfig(_))
            && !self.native_agents.creation_preconfigured.contains(&owner);
        let preparing = matches!(call, Call::Prepare);
        let Ok(ticket) = self
            .native_agents
            .owner
            .begin(owner.clone(), request_id, call, discover)
        else {
            return false;
        };
        self.native_agents.providers.insert(owner.clone(), provider);
        if capture.prepare_after_session && preparing {
            self.native_agents.creation_prepared.insert(owner.clone());
        }
        let network_ticket = ticket.clone();
        let network = crate::network::NetworkTask::spawn_on(executor, async move {
            api.call(&network_ticket).await
        });
        let stored_owner = owner.clone();
        let stored_request_id = ticket.request_id.clone();
        let creation_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = network.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this
                    .native_agents
                    .creations
                    .get(&owner)
                    .is_none_or(|(id, _)| id != &ticket.request_id)
                {
                    return;
                }
                this.native_agents.creations.remove(&owner);
                let succeeded = match result {
                    Some(Ok(reply)) => {
                        let correction = creation_correction(&ticket, &reply);
                        let success = matches!(&reply, Reply::Success { .. });
                        let completed = this.native_agents.owner.complete(&ticket, reply);
                        if completed {
                            this.record_creation_correction(&ticket, correction, cx);
                        }
                        completed && success
                    }
                    Some(Err(_)) | None => {
                        this.native_agents.owner.failed(&ticket);
                        if matches!(ticket.call, Call::SetConfig(_))
                            && this.creations.require_native_correction(&creation_key)
                        {
                            this.start_background_save(cx);
                        }
                        false
                    }
                };
                if succeeded {
                    if preconfigure {
                        this.native_agents
                            .creation_preconfigured
                            .insert(owner.clone());
                    } else if matches!(ticket.call, Call::SetConfig(_)) {
                        this.native_agents.creation_bound.insert(owner.clone());
                    }
                    this.native_agents.remember(&owner);
                    this.native_requests.dirty = true;
                    if let Some(next) = this.native_agents.waiting.get(&owner).cloned() {
                        this.resume_native_created_session(
                            &next.key,
                            next.client,
                            next.executor,
                            next.requests,
                            (window, cx),
                        );
                    }
                }
                cx.notify();
            });
        });
        self.native_agents
            .creations
            .insert(stored_owner, (stored_request_id, task));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::ids::{MessageId, PartId};
    use openchamber_native_core::tool_shells::{ToolExecution, ToolFacts};
    use openchamber_native_core::transcript::{Part, PartTime, ToolStatus};

    #[test]
    fn session_first_intent_defers_unknown_fields_then_requires_every_captured_choice() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../packages/sdk/fixtures/native-agent-v1.json"
        ))
        .expect("SDK fixture");
        let value = &fixture["sessionFirstDraft"];
        let capture = CreationConfig {
            provider_id: value["scope"]["providerID"]
                .as_str()
                .expect("provider ID")
                .into(),
            model_id: "captured-route".into(),
            variant: None,
            prepare_after_session: true,
            correction_required: false,
            config: vec![
                ("runtime".into(), "cloud".into()),
                ("live-field".into(), "deep".into()),
            ],
        };
        assert!(capture.validate());
        let mut invalid = capture.clone();
        invalid.prepare_after_session = false;
        assert!(
            !invalid.validate(),
            "legacy creation still requires its immutable variant"
        );
        let mut state = openchamber_native_core::native_agent::State {
            scope: Scope {
                directory: "/project".into(),
                provider_id: capture.provider_id.clone(),
                session_id: Some("real-session".into()),
            },
            activation_id: "real".into(),
            revision: 0,
            status: Status::Unprepared,
            authentication: openchamber_native_core::native_agent::Authentication::Unknown,
            permission_owner: PermissionOwner::Native,
            capabilities: openchamber_native_core::native_agent::Capabilities {
                authenticate: true,
                set_config: true,
                answer: false,
                tool_progress: false,
                prepare_after_session: true,
            },
            config_complete: false,
            config: vec![Config {
                id: "runtime".into(),
                label: "Runtime".into(),
                role: ConfigRole::Mode,
                value: "local".into(),
                choices: vec![
                    openchamber_native_core::native_agent::Choice {
                        id: "local".into(),
                        label: "Local".into(),
                    },
                    openchamber_native_core::native_agent::Choice {
                        id: "cloud".into(),
                        label: "Cloud".into(),
                    },
                ],
            }],
            requests: Vec::new(),
            submission: openchamber_native_core::native_agent::Submission::None,
            diagnostic: None,
            execution: None,
        };
        assert_eq!(
            capture.changes(&state, false).expect("changes"),
            vec![ConfigChange {
                id: "runtime".into(),
                value: "cloud".into()
            }]
        );
        assert!(
            capture.changes(&state, true).is_err(),
            "unknown captured IDs survive until live validation"
        );
        state.config.push(Config {
            id: "live-field".into(),
            label: "Effort".into(),
            role: ConfigRole::Effort,
            value: "normal".into(),
            choices: vec![
                openchamber_native_core::native_agent::Choice {
                    id: "normal".into(),
                    label: "Normal".into(),
                },
                openchamber_native_core::native_agent::Choice {
                    id: "deep".into(),
                    label: "Deep".into(),
                },
            ],
        });
        state.config.push(Config {
            id: "new-live-default".into(),
            label: "New".into(),
            role: ConfigRole::Other,
            value: "default".into(),
            choices: vec![openchamber_native_core::native_agent::Choice {
                id: "default".into(),
                label: "Default".into(),
            }],
        });
        assert!(
            capture.changes(&state, true).is_err(),
            "available fields alone cannot certify complete discovery"
        );
        state.config_complete = true;
        assert_eq!(
            capture.changes(&state, true).expect("changes").len(),
            2,
            "new live fields keep authoritative defaults"
        );
        state.config[1].choices.retain(|choice| choice.id != "deep");
        assert!(capture.changes(&state, true).is_err());
        assert_eq!(
            capture.config[1].1, "deep",
            "validation never rewrites frozen intent"
        );
        state.authentication = openchamber_native_core::native_agent::Authentication::Authenticated;
        state.config_complete = false;
        let key = OwnerKey {
            server: "server".into(),
            scope: state.scope.clone(),
        };
        let mut owner = NativeAgents::default();
        let mut ticket = owner
            .begin(key, "catalog-result".into(), Call::Prepare, true)
            .expect("begin");
        let invalid = Reply::Failure {
            request_id: ticket.request_id.clone(),
            code: openchamber_native_core::native_agent::ErrorCode::InvalidConfig,
            outcome: openchamber_native_core::native_agent::Outcome::Rejected,
            state: Some(state.clone()),
        };
        assert!(creation_correction(&ticket, &invalid));
        assert!(
            !invalid.state().expect("state").config_complete,
            "neither authenticated error nor Ready implies completeness"
        );
        let unavailable = Reply::Failure {
            request_id: ticket.request_id.clone(),
            code: openchamber_native_core::native_agent::ErrorCode::Unavailable,
            outcome: openchamber_native_core::native_agent::Outcome::Rejected,
            state: Some(state),
        };
        assert!(!creation_correction(&ticket, &unavailable));
        ticket.call = Call::GetState;
        assert!(creation_correction(&ticket, &invalid));
    }

    #[test]
    fn replay_gate_covers_the_cursor_frame_and_does_not_clear_on_an_unrelated_frame() {
        let mut tasks = NativeAgentTasks::default();
        tasks.connected(true, Some("cursor-2".into()));
        assert!(tasks.replaying());
        tasks.frame_applied(None);
        tasks.frame_applied(Some("cursor-1"));
        assert!(tasks.replaying());
        // Callers apply every event of cursor-2 before invoking frame_applied.
        tasks.frame_applied(Some("cursor-2"));
        assert!(!tasks.replaying());
        tasks.connected(false, Some("not-a-replay".into()));
        assert!(!tasks.replaying());
    }

    #[test]
    fn replayed_tool_transition_loses_live_authority_without_erasing_executed_identity() {
        let mut event = TranscriptEvent::ToolTransition {
            message_id: MessageId::from("assistant"),
            part_id: PartId::from("call"),
            status: ToolStatus::Running,
            output: None,
            at: 1,
            facts: Some(Box::new(ToolFacts {
                execution: Some(ToolExecution {
                    call_id: "call".into(),
                    provider_executed: true,
                    live: true,
                }),
                ..Default::default()
            })),
        };
        demote_replayed_execution(&mut event);
        let TranscriptEvent::ToolTransition {
            facts: Some(facts), ..
        } = event
        else {
            panic!("tool transition");
        };
        assert_eq!(
            facts.execution,
            Some(ToolExecution {
                call_id: "call".into(),
                provider_executed: true,
                live: false
            })
        );
        let mut part_event = TranscriptEvent::PartUpdated {
            message_id: MessageId::from("assistant"),
            part: Part {
                id: PartId::from("call"),
                time: PartTime::default(),
                content: PartContent::Tool {
                    tool: "synthetic".into(),
                    status: ToolStatus::Running,
                    title: None,
                    output: None,
                    elided: None,
                    child: None,
                    facts: Box::new(ToolFacts {
                        execution: Some(ToolExecution {
                            call_id: "call".into(),
                            provider_executed: true,
                            live: true,
                        }),
                        ..Default::default()
                    }),
                },
            },
        };
        demote_replayed_execution(&mut part_event);
        let TranscriptEvent::PartUpdated {
            part:
                Part {
                    content: PartContent::Tool { facts, .. },
                    ..
                },
            ..
        } = part_event
        else {
            panic!("tool part");
        };
        assert!(
            facts
                .execution
                .is_some_and(|execution| execution.provider_executed && !execution.live)
        );
    }

    #[test]
    fn creation_snapshot_is_bounded_unique_and_excludes_known_credential_fields() {
        let snapshot = CreationConfig {
            provider_id: "synthetic".into(),
            model_id: "local".into(),
            variant: Some("immutable-3".into()),
            prepare_after_session: false,
            correction_required: false,
            config: vec![("model-choice".into(), "opaque-model".into())],
        };
        assert!(snapshot.validate());
        let bytes = serde_json::to_vec(&snapshot).expect("serialize");
        let restored: CreationConfig = serde_json::from_slice(&bytes).expect("restore");
        assert_eq!(snapshot, restored);
        for field in ["apiKey", "access_token", "password", "nativeConfigToken"] {
            let mut invalid = snapshot.clone();
            invalid.config = vec![(field.into(), "opaque".into())];
            assert!(!invalid.validate());
        }
        let mut duplicate = snapshot.clone();
        duplicate.config.push(duplicate.config[0].clone());
        assert!(!duplicate.validate());
        let mut empty_variant = snapshot.clone();
        empty_variant.variant = Some(String::new());
        assert!(!empty_variant.validate());
        let mut oversized = snapshot;
        oversized.config[0].1 = "x".repeat(257);
        assert!(!oversized.validate());
    }
}
