//! SDK native-agent v1 records and generation-scoped client ownership.
//! JSON and I/O belong to transport. No transition here submits or retries work.

use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scope {
    pub directory: String,
    pub provider_id: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnerKey {
    /// Verified server/runtime identity, never a plugin-supplied URL.
    pub server: String,
    pub scope: Scope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Unprepared,
    Preparing,
    Ready,
    Stopped,
    Error,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authentication {
    Unknown,
    Required,
    Authenticated,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionOwner {
    OpenCode,
    Native,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigRole {
    Model,
    Effort,
    Mode,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    StaleActivation,
    StaleRevision,
    NotPrepared,
    AuthenticationRequired,
    Unsupported,
    PolicyDenied,
    RequestNotFound,
    InvalidAnswer,
    InvalidConfig,
    Unavailable,
    OutcomeUnknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Rejected,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: ErrorCode,
    pub retryable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub authenticate: bool,
    pub set_config: bool,
    pub answer: bool,
    pub tool_progress: bool,
    pub prepare_after_session: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub label: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub id: String,
    pub label: String,
    pub role: ConfigRole,
    pub value: String,
    pub choices: Vec<Choice>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionChoice {
    pub id: String,
    pub label: String,
    pub decision: Decision,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionField {
    pub id: String,
    pub label: String,
    pub required: bool,
    pub multiple: bool,
    pub allow_text: bool,
    pub choices: Vec<Choice>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Permission { choices: Vec<PermissionChoice> },
    Question { fields: Vec<QuestionField> },
}
#[derive(Clone, PartialEq, Eq)]
pub struct PendingRequest {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kind: RequestKind,
}
impl std::fmt::Debug for PendingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRequest")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct QuestionValue {
    pub field_id: String,
    pub choice_ids: Vec<String>,
    pub text: Option<String>,
}
impl std::fmt::Debug for QuestionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionValue").finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Eq)]
pub enum Answer {
    Permission {
        request_id: String,
        choice_id: String,
    },
    Question {
        request_id: String,
        values: Vec<QuestionValue>,
    },
}
impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Answer")
            .field("request_id", &self.request_id())
            .finish_non_exhaustive()
    }
}
impl Answer {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Permission { request_id, .. } | Self::Question { request_id, .. } => request_id,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigChange {
    pub id: String,
    pub value: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submission {
    None,
    Pending(String),
    OutcomeUnknown(String),
}
/// Immutable execution-bound routing triple published only after provider
/// configuration persistence and catalog publication have succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionBinding {
    pub provider_id: String,
    pub model_id: String,
    pub variant: String,
}
impl ExecutionBinding {
    pub fn model_choice(&self) -> crate::send_config::ModelChoice {
        crate::send_config::ModelChoice {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            variant: Some(self.variant.clone()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub scope: Scope,
    pub activation_id: String,
    pub revision: u64,
    pub status: Status,
    pub authentication: Authentication,
    pub permission_owner: PermissionOwner,
    pub capabilities: Capabilities,
    pub config_complete: bool,
    pub config: Vec<Config>,
    pub requests: Vec<PendingRequest>,
    pub submission: Submission,
    pub diagnostic: Option<Diagnostic>,
    pub execution: Option<ExecutionBinding>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Success {
        request_id: String,
        state: State,
    },
    Failure {
        request_id: String,
        code: ErrorCode,
        outcome: Outcome,
        state: Option<State>,
    },
}
impl Reply {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Success { request_id, .. } | Self::Failure { request_id, .. } => request_id,
        }
    }
    pub fn state(&self) -> Option<&State> {
        match self {
            Self::Success { state, .. } => Some(state),
            Self::Failure { state, .. } => state.as_ref(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    GetState,
    Prepare,
    Authenticate,
    SetConfig(Vec<ConfigChange>),
    Answer(Answer),
}
impl Call {
    pub fn method(&self) -> &'static str {
        match self {
            Self::GetState => "getState",
            Self::Prepare => "prepare",
            Self::Authenticate => "authenticate",
            Self::SetConfig(_) => "setConfig",
            Self::Answer(_) => "answer",
        }
    }
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::GetState | Self::Prepare)
    }
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.encode_utf16().count() <= 256
        && !value.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}
pub fn valid_scope(scope: &Scope) -> bool {
    let path = scope.directory.as_bytes();
    let absolute = path.first() == Some(&b'/')
        || path.starts_with(b"\\\\")
        || (path.len() >= 3
            && path[0].is_ascii_alphabetic()
            && path[1] == b':'
            && matches!(path[2], b'/' | b'\\'));
    absolute
        && !scope.directory.contains('\0')
        && scope.directory.encode_utf16().count() <= 4096
        && valid_id(&scope.provider_id)
        && scope.session_id.as_deref().is_none_or(valid_id)
}
fn unique<'a>(ids: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = BTreeSet::new();
    ids.into_iter().all(|id| seen.insert(id))
}
pub fn validate_config(config: &[Config], changes: &[ConfigChange]) -> bool {
    !changes.is_empty()
        && changes.len() <= 32
        && unique(changes.iter().map(|c| c.id.as_str()))
        && changes.iter().all(|c| {
            valid_id(&c.id)
                && valid_id(&c.value)
                && config.iter().any(|field| {
                    field.id == c.id && field.choices.iter().any(|choice| choice.id == c.value)
                })
        })
}
pub fn validate_answer(request: &PendingRequest, answer: &Answer) -> bool {
    if request.id != answer.request_id() {
        return false;
    }
    match (&request.kind, answer) {
        (RequestKind::Permission { choices }, Answer::Permission { choice_id, .. }) => {
            choices.iter().any(|c| c.id == *choice_id)
        }
        (RequestKind::Question { fields }, Answer::Question { values, .. }) => {
            values.len() <= 32
                && unique(values.iter().map(|v| v.field_id.as_str()))
                && values
                    .iter()
                    .all(|v| fields.iter().any(|f| f.id == v.field_id))
                && fields.iter().all(|field| {
                    let Some(value) = values.iter().find(|v| v.field_id == field.id) else {
                        return !field.required;
                    };
                    value.choice_ids.len() <= 256
                        && unique(value.choice_ids.iter().map(String::as_str))
                        && (field.multiple || value.choice_ids.len() <= 1)
                        && (field.allow_text || value.text.is_none())
                        && value
                            .text
                            .as_ref()
                            .is_none_or(|s| s.encode_utf16().count() <= 32_000)
                        && value
                            .choice_ids
                            .iter()
                            .all(|id| field.choices.iter().any(|c| c.id == *id))
                        && (!field.required
                            || !value.choice_ids.is_empty()
                            || value
                                .text
                                .as_ref()
                                .is_some_and(|s| !crate::form::trim_ecmascript(s).is_empty()))
                })
        }
        _ => false,
    }
}

/// An owned request captured by `begin`. Clone it for the asynchronous call;
/// only `complete`/`failed` retire its outstanding generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub key: OwnerKey,
    pub request_id: String,
    pub call: Call,
    pub activation_id: Option<String>,
    pub expected_revision: Option<u64>,
    generation: u64,
    mutation_epoch: u64,
}

#[derive(Default)]
struct Entry {
    state: Option<State>,
    outstanding: BTreeMap<String, Ticket>,
    used_ids: BTreeSet<String>,
    uncertain_answers: BTreeSet<String>,
    progress: BTreeMap<ToolIdentity, Progress>,
    failure: Option<ErrorCode>,
    valid: bool,
    discovery_id: Option<String>,
    uncertain_submission: Option<String>,
    uncertain_mutation: bool,
    mutation_epoch: u64,
    needs_discovery: bool,
}

impl Entry {
    fn advance_mutation_epoch(&mut self) {
        self.mutation_epoch = self
            .mutation_epoch
            .checked_add(1)
            .expect("native-agent mutation epoch exhausted");
    }
    fn mark_uncertain(&mut self, call: &Call) {
        if !call.is_mutation() {
            return;
        }
        self.advance_mutation_epoch();
        self.uncertain_mutation = true;
        if let Call::Answer(answer) = call {
            self.uncertain_answers
                .insert(answer.request_id().to_owned());
        }
    }
    fn withdraw(&mut self) {
        for ticket in std::mem::take(&mut self.outstanding).into_values() {
            self.mark_uncertain(&ticket.call);
        }
        for progress in self.progress.values_mut() {
            progress.visible = false;
        }
        self.valid = false;
        self.discovery_id = None;
    }
}

/// One owner retains all providers' approvals. Selecting a next-message model
/// never removes entries. `invalidate` retires calls on server/location changes.
#[derive(Default)]
pub struct NativeAgents {
    generation: u64,
    entries: BTreeMap<OwnerKey, Entry>,
}
impl std::fmt::Debug for NativeAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAgents")
            .field("generation", &self.generation)
            .field("owners", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ToolIdentity {
    pub message_id: String,
    pub part_id: String,
    pub call_id: String,
}
#[derive(Clone, PartialEq, Eq)]
pub struct ToolProgress {
    pub scope: Scope,
    pub activation_id: String,
    pub revision: u64,
    pub tool: ToolIdentity,
    pub sequence: u64,
    pub text: String,
}
impl std::fmt::Debug for ToolProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolProgress")
            .field("tool", &self.tool)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    StateChanged(State),
    ToolProgress(ToolProgress),
}
struct Progress {
    activation_id: String,
    sequence: u64,
    revision: u64,
    text: String,
    visible: bool,
}

/// Supplied only from a current authoritative OpenCode part. `live` excludes
/// reconstructed/resumed history; provider is the executing message's provider.
#[derive(Debug)]
pub struct ToolAuthority<'a> {
    pub tool: &'a ToolIdentity,
    pub provider_id: &'a str,
    pub running: bool,
    pub executed: bool,
    pub live: bool,
}

/// Resolve authority from the exact OpenCode message and part. Provider identity
/// comes from its assistant model, never websearch or plugin metadata.
pub fn tool_authority<'a>(
    transcript: &'a crate::transcript::Transcript,
    tool: &'a ToolIdentity,
) -> Option<ToolAuthority<'a>> {
    use crate::ids::MessageId;
    use crate::transcript::{PartContent, ToolStatus};
    let row = transcript.row_of(&MessageId::from(tool.message_id.as_str()))?;
    let message = transcript.message(row)?;
    let provider = &message.info.assistant.as_ref()?.model.as_ref()?.provider_id;
    let part = message
        .parts
        .iter()
        .find(|p| p.id.as_str() == tool.part_id)?;
    let PartContent::Tool { status, facts, .. } = &part.content else {
        return None;
    };
    let execution = facts.execution.as_ref()?;
    if execution.call_id != tool.call_id {
        return None;
    }
    Some(ToolAuthority {
        tool,
        provider_id: provider,
        running: *status == ToolStatus::Running,
        executed: execution.provider_executed,
        live: execution.live,
    })
}

impl NativeAgents {
    pub fn state(&self, key: &OwnerKey) -> Option<&State> {
        self.entries.get(key)?.state.as_ref()
    }
    pub fn requires_discovery(&self, key: &OwnerKey) -> bool {
        self.entries
            .get(key)
            .is_none_or(|entry| !entry.valid || entry.needs_discovery)
    }
    pub fn failure(&self, key: &OwnerKey) -> Option<ErrorCode> {
        self.entries.get(key)?.failure
    }
    /// Retained uncertainty can outlive an activation whose first snapshot no
    /// longer names the previous submission. A newer authoritative snapshot
    /// in the accepted activation must reconcile it before another send.
    pub fn submission_uncertain(&self, key: &OwnerKey) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.uncertain_submission.is_some())
    }
    pub fn states(&self) -> impl Iterator<Item = (&OwnerKey, &State)> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| entry.state.as_ref().map(|s| (key, s)))
    }
    pub fn invalidate(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("native-agent generation exhausted");
        for entry in self.entries.values_mut() {
            entry.withdraw();
        }
    }
    /// `discover` must be explicit for replacement activations. Events cannot
    /// establish one. Provider-policy enforcement also runs in the plugin.
    pub fn begin(
        &mut self,
        key: OwnerKey,
        request_id: String,
        call: Call,
        discover: bool,
    ) -> Result<Ticket, ErrorCode> {
        if !valid_scope(&key.scope)
            || key.server.is_empty()
            || !valid_id(&request_id)
            || (discover && call.is_mutation())
        {
            return Err(ErrorCode::InvalidRequest);
        }
        let entry = self.entries.entry(key.clone()).or_default();
        if entry.needs_discovery && !discover {
            return Err(ErrorCode::StaleActivation);
        }
        if entry.used_ids.contains(&request_id) {
            return Err(ErrorCode::InvalidRequest);
        }
        if call.is_mutation() {
            if !entry.valid {
                return Err(ErrorCode::NotPrepared);
            }
            if entry.uncertain_mutation {
                return Err(ErrorCode::OutcomeUnknown);
            }
            let state = entry.state.as_ref().ok_or(ErrorCode::NotPrepared)?;
            if entry.outstanding.values().any(|t| t.call.is_mutation()) {
                return Err(ErrorCode::Unavailable);
            }
            match &call {
                Call::Authenticate
                    if state.capabilities.prepare_after_session
                        && key.scope.session_id.is_none() =>
                {
                    return Err(ErrorCode::NotPrepared);
                }
                Call::Authenticate if !state.capabilities.authenticate => {
                    return Err(ErrorCode::Unsupported);
                }
                Call::SetConfig(changes) => {
                    if !state.capabilities.set_config {
                        return Err(ErrorCode::Unsupported);
                    }
                    if !validate_config(&state.config, changes) {
                        return Err(ErrorCode::InvalidConfig);
                    }
                }
                Call::Answer(answer) => {
                    if state.permission_owner != PermissionOwner::Native
                        || !state.capabilities.answer
                    {
                        return Err(ErrorCode::Unsupported);
                    }
                    if entry.uncertain_answers.contains(answer.request_id()) {
                        return Err(ErrorCode::OutcomeUnknown);
                    }
                    let request = state
                        .requests
                        .iter()
                        .find(|r| r.id == answer.request_id())
                        .ok_or(ErrorCode::RequestNotFound)?;
                    if !validate_answer(request, answer) {
                        return Err(ErrorCode::InvalidAnswer);
                    }
                }
                _ => {}
            }
            entry.advance_mutation_epoch();
        }
        let ticket = Ticket {
            key,
            request_id: request_id.clone(),
            call,
            activation_id: if discover {
                None
            } else {
                entry.state.as_ref().map(|s| s.activation_id.clone())
            },
            expected_revision: entry.state.as_ref().map(|s| s.revision),
            generation: self.generation,
            mutation_epoch: entry.mutation_epoch,
        };
        entry.used_ids.insert(request_id.clone());
        if ticket.activation_id.is_none() {
            entry.discovery_id = Some(request_id.clone());
        }
        entry.outstanding.insert(request_id, ticket.clone());
        Ok(ticket)
    }
    fn retire(&mut self, ticket: &Ticket) -> Option<&mut Entry> {
        if ticket.generation != self.generation {
            return None;
        }
        let entry = self.entries.get_mut(&ticket.key)?;
        if entry.outstanding.get(&ticket.request_id) != Some(ticket) {
            return None;
        }
        entry.outstanding.remove(&ticket.request_id);
        Some(entry)
    }
    pub fn failed(&mut self, ticket: &Ticket) -> bool {
        let Some(entry) = self.retire(ticket) else {
            return false;
        };
        entry.failure = Some(ErrorCode::Unavailable);
        entry.mark_uncertain(&ticket.call);
        true
    }
    /// Returns whether this outstanding operation completed in the current
    /// generation. Read `state` afterward; a stale snapshot can be ignored even
    /// when its operation completed successfully.
    pub fn complete(&mut self, ticket: &Ticket, reply: Reply) -> bool {
        if reply.request_id() != ticket.request_id
            || reply.state().is_some_and(|s| s.scope != ticket.key.scope)
        {
            self.failed(ticket);
            return false;
        }
        let Some(entry) = self.retire(ticket) else {
            return false;
        };
        if ticket.activation_id.is_none()
            && entry.discovery_id.as_deref() != Some(&ticket.request_id)
        {
            return false;
        }
        entry.failure = match &reply {
            Reply::Success { .. } => None,
            Reply::Failure { code, .. } => Some(*code),
        };
        if let Reply::Failure {
            outcome: Outcome::Unknown,
            ..
        } = &reply
        {
            entry.mark_uncertain(&ticket.call);
        }
        if matches!(
            &reply,
            Reply::Failure {
                code: ErrorCode::StaleActivation,
                ..
            }
        ) || ticket.activation_id.as_ref().is_some_and(|activation| {
            reply
                .state()
                .is_some_and(|state| state.activation_id != *activation)
        }) {
            entry.withdraw();
            entry.needs_discovery = true;
            entry.failure = Some(ErrorCode::StaleActivation);
            return true;
        }
        if let Some(state) = reply.state() {
            let discovery = !ticket.call.is_mutation() && ticket.activation_id.is_none();
            let fresh = entry.state.as_ref().is_none_or(|held| {
                if held.activation_id == state.activation_id {
                    state.revision >= held.revision
                } else {
                    discovery
                }
            });
            Self::adopt(entry, state.clone(), discovery);
            if fresh {
                entry.valid = true;
                if discovery {
                    entry.needs_discovery = false;
                }
                if !ticket.call.is_mutation()
                    && ticket.mutation_epoch == entry.mutation_epoch
                    && !entry.outstanding.values().any(|t| t.call.is_mutation())
                {
                    entry.uncertain_mutation = false;
                }
            }
        }
        true
    }
    fn adopt(entry: &mut Entry, state: State, discovery: bool) -> bool {
        let same_activation = entry
            .state
            .as_ref()
            .is_some_and(|s| s.activation_id == state.activation_id);
        if let Some(held) = &entry.state {
            if held.activation_id != state.activation_id {
                if !discovery {
                    return false;
                }
                if let Submission::Pending(id) | Submission::OutcomeUnknown(id) = &held.submission {
                    entry.uncertain_submission = Some(id.clone());
                }
                // Other responses from the replaced lifetime lose authority.
                entry.withdraw();
                entry.progress.clear();
            } else if state.revision <= held.revision {
                return false;
            }
        } else if !discovery {
            return false;
        }
        entry
            .uncertain_answers
            .retain(|id| state.requests.iter().any(|r| r.id == *id));
        match &state.submission {
            Submission::OutcomeUnknown(id) => entry.uncertain_submission = Some(id.clone()),
            Submission::None if same_activation => entry.uncertain_submission = None,
            _ => {}
        }
        entry.state = Some(state);
        true
    }
    pub fn state_changed(&mut self, server: &str, state: State) -> bool {
        let key = OwnerKey {
            server: server.to_owned(),
            scope: state.scope.clone(),
        };
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        if !entry.valid {
            return false;
        }
        Self::adopt(entry, state, false)
    }
    fn live_matches(key: &OwnerKey, tool: &ToolAuthority<'_>) -> bool {
        tool.live
            && tool.running
            && tool.executed
            && tool.provider_id == key.scope.provider_id
            && key.scope.session_id.is_some()
    }
    pub fn tool_progress(
        &mut self,
        key: &OwnerKey,
        progress: ToolProgress,
        tool: &ToolAuthority<'_>,
    ) -> bool {
        if !Self::live_matches(key, tool)
            || progress.scope != key.scope
            || progress.tool != *tool.tool
        {
            return false;
        }
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        if !entry.valid {
            return false;
        }
        let Some(state) = &entry.state else {
            return false;
        };
        if !state.capabilities.tool_progress
            || progress.activation_id != state.activation_id
            || progress.revision != state.revision
        {
            return false;
        }
        if entry.progress.get(&progress.tool).is_some_and(|p| {
            p.activation_id == progress.activation_id && p.sequence >= progress.sequence
        }) {
            return false;
        }
        entry.progress.insert(
            progress.tool,
            Progress {
                activation_id: progress.activation_id,
                sequence: progress.sequence,
                revision: progress.revision,
                text: progress.text,
                visible: true,
            },
        );
        true
    }
    /// Final/failed parts and history always display OpenCode's own content.
    pub fn progress_text(&self, key: &OwnerKey, tool: &ToolAuthority<'_>) -> Option<&str> {
        if !Self::live_matches(key, tool) {
            return None;
        }
        let entry = self.entries.get(key)?;
        if !entry.valid {
            return None;
        }
        let state = entry.state.as_ref()?;
        let progress = entry.progress.get(tool.tool)?;
        (progress.visible
            && progress.activation_id == state.activation_id
            && progress.revision == state.revision)
            .then_some(progress.text.as_str())
    }
    fn mutation_authority(&self, key: &OwnerKey) -> Option<(&Entry, &State)> {
        let entry = self.entries.get(key)?;
        if !entry.valid
            || entry.needs_discovery
            || entry.uncertain_mutation
            || entry
                .outstanding
                .values()
                .any(|ticket| ticket.call.is_mutation())
        {
            return None;
        }
        Some((entry, entry.state.as_ref()?))
    }
    /// Whether this owner can answer the currently pending request. The answer
    /// payload is still validated by `begin` when the user submits it.
    pub fn can_answer(&self, key: &OwnerKey, request_id: &str) -> bool {
        self.mutation_authority(key).is_some_and(|(entry, state)| {
            state.permission_owner == PermissionOwner::Native
                && state.capabilities.answer
                && !entry.uncertain_answers.contains(request_id)
                && state
                    .requests
                    .iter()
                    .any(|request| request.id == request_id)
        })
    }
    /// Config authority does not require an execution binding or authentication.
    /// `begin` validates the selected changes against the current config.
    pub fn can_set_config(&self, key: &OwnerKey) -> bool {
        self.mutation_authority(key)
            .is_some_and(|(_, state)| state.capabilities.set_config)
    }
    /// Config-only draft authority permits creating a real OpenCode session;
    /// it grants no authority to dispatch a native prompt or authenticate.
    pub fn can_prepare_after_session(&self, key: &OwnerKey) -> bool {
        key.scope.session_id.is_none()
            && self.mutation_authority(key).is_some_and(|(entry, state)| {
                state.capabilities.prepare_after_session
                    && state.status == Status::Unprepared
                    && state.execution.is_none()
                    && matches!(state.submission, Submission::None)
                    && entry.uncertain_submission.is_none()
            })
    }
    /// Authentication authority is available before authenticated submission.
    pub fn can_authenticate(&self, key: &OwnerKey) -> bool {
        self.mutation_authority(key).is_some_and(|(_, state)| {
            state.capabilities.authenticate
                && (!state.capabilities.prepare_after_session || key.scope.session_id.is_some())
        })
    }
    pub fn can_submit(&self, key: &OwnerKey) -> bool {
        self.entries.get(key).is_some_and(|e| {
            e.valid
                && !e.uncertain_mutation
                && !e.outstanding.values().any(|t| t.call.is_mutation())
                && e.uncertain_submission.is_none()
                && e.state.as_ref().is_some_and(|s| {
                    s.status == Status::Ready
                        && (!s.capabilities.prepare_after_session || s.config_complete)
                        && (!s.capabilities.prepare_after_session || key.scope.session_id.is_some())
                        && s.authentication == Authentication::Authenticated
                        && matches!(s.submission, Submission::None)
                        && s.execution.is_some()
                })
        })
    }
}

#[cfg(test)]
mod tests;
