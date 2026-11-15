//! The starter chips of the new-session view (`DraftPresetChips.tsx` with
//! `useDraftStarters.ts`): which chips show, what a chip submits, and the
//! edits that pin, remove, reorder and share them.
//!
//! A chip submits at once: the starter, then the composer's text, creates
//! the selected draft's session as Enter would, after Chat directory and native
//! provider preparation. A built-in starter is one
//! of the composer's prompt-pair commands; it renders its visible prompt and
//! its instructions from the overrides the server resolves for the project,
//! read for each activation, over the built-in templates the resource
//! generator exports. A failed read sends nothing.

use std::collections::HashMap;
use std::sync::OnceLock;

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{AnyWindowHandle, AppContext, Context, Task, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::draft_starters::{
    self, Available, ProjectStarter, ResolvedStarter, StarterGroup, StarterRef, StarterSource,
};
use openchamber_native_core::ids::MessageId;
use openchamber_native_core::magic_prompt::{
    self, CommandVariable, MagicCommand, MagicPrompts, PromptSide,
};
use openchamber_native_transport::client::SettingsChanges;
use openchamber_native_transport::prompt_parts::PromptParts;
use serde::Deserialize;

use crate::locale;
use crate::starter_capture::{StarterCapture, StarterCaptures};
use crate::workspace::Workspace;

const MAGIC_PROMPTS_JSON: &str = include_str!("../resources/magic-prompts.json");

#[cfg(test)]
#[path = "starter_continuation_tests.rs"]
mod continuation_tests;

/// A starter's first prompt for a new session.
pub(crate) struct StarterPrompt {
    /// The retained owner transferred atomically into creation.
    pub capture_id: MessageId,
    /// Full preset rollback content, independent of the current composer.
    pub draft: Draft,
    /// The rendered prompt the session receives.
    pub prompt_text: String,
    pub parts: PromptParts,
    /// Memory-only user intent from activation, before template preparation.
    pub effort: Option<crate::effort_preferences::UserEffort>,
    pub native_config: Option<crate::native_agent::CreationConfig>,
}

/// Live continuation only. The journal recovers the complete capture as a draft
/// after restart; it never replays this activation.
struct StarterContinuation {
    window: AnyWindowHandle,
    rendered: Option<(String, String)>,
    effort: Option<crate::effort_preferences::UserEffort>,
    native: Option<StarterNative>,
}

struct StarterNative {
    /// Existing choices are captured even when the provider is not ready yet.
    /// None means preparation has not published any choices to the user.
    preferences: Option<Vec<(String, String)>>,
    /// A published execution binding and its preferences are immutable.
    config: Option<crate::native_agent::CreationConfig>,
    preparation: Option<u64>,
}

fn starter_native_config(
    execution: &openchamber_native_core::native_agent::ExecutionBinding,
    preferences: Vec<(String, String)>,
) -> crate::native_agent::CreationConfig {
    crate::native_agent::CreationConfig {
        provider_id: execution.provider_id.clone(),
        model_id: execution.model_id.clone(),
        variant: Some(execution.variant.clone()),
        prepare_after_session: false,
        correction_required: false,
        config: preferences,
    }
}

/// Authentication can complete through the native provider's existing controls.
/// A transport error or a terminal preparation response restores the source.
pub(crate) fn native_preparation_failed(
    outcome: Option<Result<(), openchamber_native_core::native_agent::ErrorCode>>,
    state: Option<&openchamber_native_core::native_agent::State>,
) -> bool {
    use openchamber_native_core::native_agent::{ErrorCode, Status};
    match outcome {
        None => true,
        Some(Err(code)) => code != ErrorCode::AuthenticationRequired,
        // A successful reply may contain an older snapshot than a live event.
        Some(Ok(())) => {
            state.is_none_or(|state| matches!(state.status, Status::Error | Status::Stopped))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandResource {
    name: String,
    visible: String,
    instructions: String,
    requires_session: bool,
    error_toast_key: String,
    variables: Vec<VariableResource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VariableResource {
    side: String,
    key: String,
    empty: String,
    with_argument: String,
}

#[derive(Deserialize)]
struct MagicResource {
    commands: Vec<CommandResource>,
    templates: HashMap<String, String>,
}

/// The prompt-pair commands and their built-in templates.
pub(crate) struct MagicCatalog {
    commands: Vec<(MagicCommand, String)>,
    templates: HashMap<String, String>,
}

impl MagicCatalog {
    pub(crate) fn get() -> &'static Self {
        static CATALOG: OnceLock<MagicCatalog> = OnceLock::new();
        CATALOG.get_or_init(|| {
            let resource: MagicResource = serde_json::from_str(MAGIC_PROMPTS_JSON)
                .expect("resources/magic-prompts.json is generated JSON");
            let commands = resource
                .commands
                .into_iter()
                .map(|command| {
                    let variables = command
                        .variables
                        .into_iter()
                        .map(|variable| CommandVariable {
                            side: if variable.side == "instructions" {
                                PromptSide::Instructions
                            } else {
                                PromptSide::Visible
                            },
                            key: variable.key,
                            empty: variable.empty,
                            with_argument: variable.with_argument,
                        })
                        .collect();
                    (
                        MagicCommand {
                            name: command.name,
                            visible: command.visible,
                            instructions: command.instructions,
                            requires_session: command.requires_session,
                            variables,
                        },
                        command.error_toast_key,
                    )
                })
                .collect();
            Self {
                commands,
                templates: resource.templates,
            }
        })
    }

    /// The built-in templates by prompt ID.
    pub(crate) fn templates(&self) -> &HashMap<String, String> {
        &self.templates
    }

    /// The command named `name` and the locale key of its failure notice.
    pub(crate) fn command(&self, name: &str) -> Option<(&MagicCommand, &str)> {
        self.commands
            .iter()
            .find(|(command, _)| command.name == name)
            .map(|(command, key)| (command, key.as_str()))
    }

    /// The visible prompt and the instructions of one invocation.
    pub(crate) fn render(
        &self,
        command: &MagicCommand,
        argument: &str,
        overrides: &HashMap<String, String>,
    ) -> Option<(String, String)> {
        let prompts = MagicPrompts {
            overrides,
            defaults: &self.templates,
        };
        let variables = command.variables(argument);
        Some((
            prompts.render(&command.visible, &variables.visible)?,
            prompts.render(&command.instructions, &variables.instructions)?,
        ))
    }
}

/// A project's starters as last read.
enum ProjectStarters {
    Loading,
    /// The merged list, shared first.
    Ready(Vec<ProjectStarter>),
    /// The read failed: nothing shows and nothing is written back, since a
    /// list written after a failed read would replace what is on disk. The
    /// read runs again at `retry_at`.
    Failed {
        retry_at: std::time::Instant,
    },
}

/// The first wait before a failed project read runs again; each further
/// failure doubles it up to `READ_RETRY_MAX`.
const READ_RETRY: std::time::Duration = std::time::Duration::from_secs(2);
const READ_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// The starters' state for the current runtime.
#[derive(Default)]
pub(crate) struct Starters {
    /// The global list after an edit, until the saved settings answer it.
    global_edit: Option<Vec<StarterRef>>,
    global_saving: bool,
    global_pending: Option<Vec<StarterRef>>,
    project: HashMap<String, ProjectStarters>,
    project_saving: HashMap<String, Option<Vec<StarterRef>>>,
    /// The newest edit of a project's lists made while a save was out: the
    /// user's own list and, when it changed too, the shared one.
    project_pending: HashMap<String, (Vec<StarterRef>, Option<Vec<StarterRef>>)>,
    /// Consecutive failed reads per project, for the retry backoff.
    project_failures: HashMap<String, u32>,
    /// Each pending activation owns its complete source independently of typing.
    pub(crate) captures: StarterCaptures,
    /// Override reads and completed continuations retain the original owner
    /// through runtime switches. Only revisiting that owner can create a session.
    capture_tasks: HashMap<MessageId, Task<()>>,
    continuations: HashMap<MessageId, StarterContinuation>,
    generation: u64,
    tasks: HashMap<u64, Task<()>>,
    next_task: u64,
}

impl Starters {
    pub(crate) fn capture_pending(&self, id: &MessageId) -> bool {
        self.capture_tasks.contains_key(id) || self.continuations.contains_key(id)
    }

    pub(crate) fn discard_captures(&mut self, owner: &DraftKey) {
        for capture in self.captures.discard_owner(owner) {
            self.capture_tasks.remove(&capture.id);
            self.continuations.remove(&capture.id);
        }
    }

    /// Invalidates old runtime projections while retaining all captured ownership.
    pub(crate) fn reset(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            captures: std::mem::take(&mut self.captures),
            capture_tasks: std::mem::take(&mut self.capture_tasks),
            continuations: std::mem::take(&mut self.continuations),
            next_task: self.next_task,
            ..Self::default()
        };
    }

    fn task(&mut self, task: impl FnOnce(u64) -> Task<()>) {
        let id = self.next_task;
        self.next_task += 1;
        let task = task(id);
        self.tasks.insert(id, task);
    }
}

impl Workspace {
    /// Whether the settings show the starter row (`draftStartersVisible`).
    pub(crate) fn starters_visible(&self) -> bool {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.draft_starters_visible)
            != Some(false)
    }

    /// The global list: an edit in flight, else the saved one, else every
    /// built-in.
    fn global_starters(&self) -> Vec<StarterRef> {
        self.starters
            .global_edit
            .clone()
            .or_else(|| {
                self.send_catalog
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.draft_starters.clone())
            })
            .unwrap_or_else(draft_starters::default_global)
    }

    /// The registered project of the new-session draft.
    fn starter_project(&self) -> Option<String> {
        let owner = self.composer_key.as_ref()?;
        if !matches!(owner.target(), DraftTarget::NewSession(_)) || self.selected.is_some() {
            return None;
        }
        self.topology
            .projects
            .iter()
            .find(|project| project.path == owner.directory())
            .map(|project| project.path.clone())
    }

    /// The chips: the global group, then the project's.
    pub(crate) fn resolved_starters(&self) -> Vec<ResolvedStarter> {
        // Native lists no commands or skills, so only built-ins resolve;
        // other entries stay in the saved lists untouched.
        let available = Available::default();
        let mut chips: Vec<ResolvedStarter> = self
            .global_starters()
            .iter()
            .filter_map(|starter| {
                draft_starters::resolve(starter, StarterGroup::Global, false, &available)
            })
            .collect();
        if let Some(ProjectStarters::Ready(project)) = self
            .starter_project()
            .and_then(|path| self.starters.project.get(&path))
        {
            chips.extend(project.iter().filter_map(|entry| {
                draft_starters::resolve(
                    &entry.starter,
                    StarterGroup::Project,
                    entry.source == StarterSource::Shared,
                    &available,
                )
            }));
        }
        chips
    }

    /// The built-ins not pinned anywhere, for the add picker.
    pub(crate) fn pinnable_starters(&self) -> Vec<StarterRef> {
        let global = self.global_starters();
        let project: Vec<StarterRef> = match self
            .starter_project()
            .and_then(|path| self.starters.project.get(&path))
        {
            Some(ProjectStarters::Ready(project)) => {
                project.iter().map(|entry| entry.starter.clone()).collect()
            }
            _ => Vec::new(),
        };
        draft_starters::BUILTIN_STARTERS
            .iter()
            .map(|starter| StarterRef::command(starter.name))
            .filter(|starter| !global.contains(starter) && !project.contains(starter))
            .collect()
    }

    /// Reads the draft project's starters once per runtime.
    pub(crate) fn ensure_project_starters(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.starter_project() else {
            return;
        };
        match self.starters.project.get(&path) {
            Some(ProjectStarters::Failed { retry_at })
                if std::time::Instant::now() >= *retry_at => {}
            Some(_) => return,
            None => {}
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        self.starters
            .project
            .insert(path.clone(), ProjectStarters::Loading);
        let read_path = path.clone();
        let read = connection
            .network
            .run(async move { client.project_starters(&read_path).await });
        let generation = self.starters.generation;
        self.starters.task(|id| {
            cx.spawn(async move |this, cx| {
                let result = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.starters.tasks.remove(&id);
                    if generation != this.starters.generation {
                        return;
                    }
                    let state = match result {
                        Some(Ok(list)) => {
                            this.starters.project_failures.remove(&path);
                            ProjectStarters::Ready(list)
                        }
                        failure => {
                            tracing::warn!(error = ?failure.and_then(Result::err), "the project's starters could not be read");
                            let failures = this.starters.project_failures.entry(path.clone()).or_insert(0);
                            let delay = READ_RETRY
                                .saturating_mul(1 << (*failures).min(5))
                                .min(READ_RETRY_MAX);
                            *failures += 1;
                            // Render again when the wait ends, so the read runs.
                            this.starters.task(|id| {
                                cx.spawn(async move |this, cx| {
                                    cx.background_executor().timer(delay).await;
                                    let _ = this.update(cx, |this, cx| {
                                        this.starters.tasks.remove(&id);
                                        cx.notify();
                                    });
                                })
                            });
                            ProjectStarters::Failed {
                                retry_at: std::time::Instant::now() + delay,
                            }
                        }
                    };
                    this.starters.project.insert(path, state);
                    cx.notify();
                });
            })
        });
    }

    fn chip(&self, id: &str) -> Option<ResolvedStarter> {
        self.resolved_starters()
            .into_iter()
            .find(|chip| chip.id == id)
    }

    /// Submits a chip: the starter with the composer's text after it
    /// (`submitPresetPrompt`).
    pub(crate) fn activate_starter(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.new_session_view_active() || !self.persistence_ready() || self.close_in_progress()
        {
            return;
        }
        let Some(chip) = self.chip(id) else {
            return;
        };
        let Some(source) = self.composer_key.clone() else {
            return;
        };
        if source.runtime() != &self.runtime
            || !matches!(source.target(), DraftTarget::NewSession(_))
            || !self.creations.can_begin(&source)
            || !self.starters.captures.can_begin(&source)
        {
            return;
        }
        let draft = self.composer_draft(cx);
        let text = draft_starters::submission(&chip, &draft.text);
        let catalog = MagicCatalog::get();
        let Some((command, error_key)) = magic_prompt::parse_slash_command(&text)
            .and_then(|parsed| catalog.command(&parsed.name).map(|found| (parsed, found)))
            .filter(|(_, (command, _))| !command.requires_session)
            .map(|(parsed, (command, key))| ((command.clone(), parsed.argument), key.to_owned()))
        else {
            // Only built-ins resolve here, and each is a prompt pair.
            return;
        };
        let project = if self.chat_draft_open() {
            if !self.is_chat_creation(&source) {
                self.retry_failed_chat_roots(window, cx);
                return;
            }
            openchamber_native_core::chat_draft::CHAT_PROJECT_ID.to_owned()
        } else {
            let Some(project) = self.draft_destination_project(source.directory()) else {
                return;
            };
            project.id.to_string()
        };
        let Some((directory, mut selection)) = self.catalog_selection() else {
            return;
        };
        let Some(model) = selection.model.as_ref() else {
            return;
        };
        let native = if self.native_provider_advertised(&model.provider_id) {
            let key = openchamber_native_core::native_agent::OwnerKey {
                server: source.runtime().as_str().to_owned(),
                scope: openchamber_native_core::native_agent::Scope {
                    directory: source.directory().to_owned(),
                    provider_id: model.provider_id.clone(),
                    session_id: None,
                },
            };
            let state = self.native_agents.owner.state(&key);
            let preferences: Option<Vec<_>> = state.map(|state| {
                state
                    .config
                    .iter()
                    .map(|field| (field.id.clone(), field.value.clone()))
                    .collect()
            });
            let config = self.capture_native_creation(model).or_else(|| {
                state
                    .and_then(|state| state.execution.as_ref())
                    .map(|execution| {
                        selection.model = Some(execution.model_choice());
                        starter_native_config(execution, preferences.clone().unwrap_or_default())
                    })
            });
            Some(StarterNative {
                preferences,
                config,
                preparation: None,
            })
        } else {
            None
        };
        if native
            .as_ref()
            .and_then(|native| native.config.as_ref())
            .is_some_and(|config| !config.validate())
        {
            return;
        }
        let effort = selection
            .model
            .as_ref()
            .map(|model| self.capture_user_effort(model));
        // Mint every admission identity and resolve every staged file before the
        // read starts. Later typing, attachment preparation and choices are separate.
        let (capture_id, _) = self.message_ids.next();
        let Ok(parts) = self.capture_draft_parts(&source, &draft) else {
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
            return;
        };
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == crate::connection::ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let executor = connection.network.handle();
        self.save_composer_draft(cx);
        let capture = StarterCapture {
            id: capture_id.clone(),
            owner: source.clone(),
            project,
            draft,
            preset_text: text,
            selection,
            parts,
            permission_mode: self.permission_modes.drafts.get(source.runtime()).copied(),
        };
        if !self.starters.captures.begin(capture, &mut self.drafts) {
            return;
        }
        self.permission_modes.drafts.remove(source.runtime());
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        self.save_draft_preview(&source, &Draft::default(), cx);
        self.starters.continuations.insert(
            capture_id.clone(),
            StarterContinuation {
                window: window.window_handle(),
                rendered: None,
                effort,
                native,
            },
        );
        self.queue_persistence(cx);
        let read_directory = directory.clone();
        let read = crate::network::NetworkTask::spawn_on(&executor, async move {
            client.magic_prompt_overrides(Some(&read_directory)).await
        });
        let task_id = capture_id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starters.capture_tasks.remove(&capture_id);
                if this.starters.captures.get(&capture_id).is_none() {
                    return;
                }
                let (command, argument) = command;
                let rendered = match result {
                    Some(Ok(overrides)) => catalog.render(&command, &argument, &overrides),
                    failure => {
                        tracing::warn!(error = ?failure.and_then(Result::err), "the magic prompts could not be read");
                        None
                    }
                };
                let Some(rendered) = rendered else {
                    this.starters.continuations.remove(&capture_id);
                    this.restore_starter_capture(&capture_id, window, cx);
                    if source.runtime() == &this.runtime {
                        window.push_notification(
                            Notification::error(locale::strings(cx).get(&error_key)),
                            cx,
                        );
                    }
                    cx.notify();
                    return;
                };
                if let Some(continuation) = this.starters.continuations.get_mut(&capture_id) {
                    continuation.rendered = Some(rendered);
                }
                this.resume_starter_activation(window, cx);
                cx.notify();
            });
        });
        self.starters.capture_tasks.insert(task_id, task);
        if self.chat_draft_open() {
            // The preparation barrier includes the capture before mkdir starts.
            self.prepare_chat_draft(window, cx);
        }
        self.resume_starter_activation(window, cx);
        cx.notify();
    }

    /// Directory completion belongs to its captured owner, including while hidden.
    pub(crate) fn starter_chat_prepared(
        &mut self,
        owner: &DraftKey,
        succeeded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !succeeded {
            let id = self
                .starters
                .captures
                .iter()
                .find(|capture| &capture.owner == owner)
                .map(|capture| capture.id.clone());
            if let Some(id) = id {
                self.starters.capture_tasks.remove(&id);
                self.starters.continuations.remove(&id);
                self.restore_starter_capture(&id, window, cx);
            }
        } else {
            self.resume_starter_activation(window, cx);
        }
    }

    /// Called after readiness changes and when the original new-session view is
    /// shown again. No timer or background create follows a different selection.
    pub(crate) fn resume_starter_activation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.new_session_view_active() || !self.persistence_ready() || self.close_in_progress()
        {
            return;
        }
        let Some(owner) = self
            .composer_key
            .clone()
            .filter(|key| key.runtime() == &self.runtime)
        else {
            return;
        };
        let Some(id) = self
            .starters
            .captures
            .iter()
            .find(|capture| capture.owner == owner)
            .map(|capture| capture.id.clone())
        else {
            return;
        };
        let Some(continuation) = self.starters.continuations.get(&id) else {
            return;
        };
        if self.is_chat_creation(&owner) && !self.chat_creation_ready(&owner) {
            return;
        }
        let Some(connection) = self.connection.as_ref().filter(|connection| {
            connection.state == crate::connection::ConnectionState::Connected
                && connection.client.is_some()
        }) else {
            return;
        };
        let generation = connection.generation;
        if let Some(native) = &continuation.native {
            let capture = self
                .starters
                .captures
                .get(&id)
                .expect("retained activation");
            let Some(model) = capture.selection.model.as_ref() else {
                return;
            };
            let key = openchamber_native_core::native_agent::OwnerKey {
                server: capture.owner.runtime().as_str().to_owned(),
                scope: openchamber_native_core::native_agent::Scope {
                    directory: capture.owner.directory().to_owned(),
                    provider_id: model.provider_id.clone(),
                    session_id: None,
                },
            };
            if self.native_action_pending(&key) {
                if !self.native_agents.replaying() {
                    // Join preparation already started by the composer. Its
                    // failure must not trigger another automatic Prepare.
                    self.starters
                        .continuations
                        .get_mut(&id)
                        .expect("live continuation")
                        .native
                        .as_mut()
                        .expect("native continuation")
                        .preparation = Some(generation);
                }
                return;
            }
            if native
                .config
                .as_ref()
                .is_some_and(|config| config.prepare_after_session)
                || self.native_agents.owner.can_prepare_after_session(&key)
            {
                // Session-first intent has no draft execution binding.
                if native.config.is_none() {
                    self.retain_starter_native_ready(&key, cx);
                    if self
                        .starters
                        .continuations
                        .get(&id)
                        .and_then(|next| next.native.as_ref())
                        .is_none_or(|native| native.config.is_none())
                    {
                        return;
                    }
                }
            } else if self.native_agents.owner.can_submit(&key) {
                if native.config.is_none() {
                    self.retain_starter_native_ready(&key, cx);
                    if self
                        .starters
                        .continuations
                        .get(&id)
                        .and_then(|next| next.native.as_ref())
                        .is_none_or(|native| native.config.is_none())
                    {
                        return;
                    }
                }
            } else {
                if native.preparation != Some(generation)
                    && self.native_call(
                        key,
                        openchamber_native_core::native_agent::Call::Prepare,
                        true,
                        cx,
                    )
                {
                    self.starters
                        .continuations
                        .get_mut(&id)
                        .expect("live continuation")
                        .native
                        .as_mut()
                        .expect("native continuation")
                        .preparation = Some(generation);
                }
                return;
            }
        }
        let capture = self
            .starters
            .captures
            .get(&id)
            .expect("retained activation");
        if !self.can_create_starter(capture) {
            return;
        }
        if self
            .starters
            .continuations
            .get(&id)
            .is_none_or(|next| next.rendered.is_none())
        {
            return;
        }
        let Ok(draft) = capture.rollback_draft() else {
            self.starters.continuations.remove(&id);
            self.restore_starter_capture(&id, window, cx);
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
            return;
        };
        let next = self
            .starters
            .continuations
            .remove(&id)
            .expect("ready continuation");
        let (visible, instructions) = next.rendered.expect("rendered prompt");
        let parts = capture.rendered_parts(instructions);
        if !self.submit_new_session_with(
            Some(StarterPrompt {
                capture_id: id.clone(),
                draft,
                prompt_text: visible,
                parts,
                effort: next.effort,
                native_config: next.native.and_then(|native| native.config),
            }),
            window,
            cx,
        ) {
            self.restore_starter_capture(&id, window, cx);
        }
        cx.notify();
    }

    /// Freeze the first ready binding even if its owner is hidden when the
    /// native response arrives. Later preference edits belong to later sends.
    pub(crate) fn retain_starter_native_ready(
        &mut self,
        key: &openchamber_native_core::native_agent::OwnerKey,
        cx: &mut Context<Self>,
    ) {
        let deferred = self.native_agents.owner.can_prepare_after_session(key);
        if key.scope.session_id.is_some() || !(deferred || self.native_agents.owner.can_submit(key))
        {
            return;
        }
        let Some(state) = self.native_agents.owner.state(key) else {
            return;
        };
        let Some(id) = self
            .starters
            .captures
            .iter()
            .find(|capture| {
                capture.owner.runtime().as_str() == key.server
                    && capture.owner.directory() == key.scope.directory
                    && capture
                        .selection
                        .model
                        .as_ref()
                        .is_some_and(|model| model.provider_id == key.scope.provider_id)
            })
            .map(|capture| capture.id.clone())
        else {
            return;
        };
        let capture = self
            .starters
            .captures
            .get(&id)
            .expect("retained activation");
        let Some(model) = capture.selection.model.as_ref() else {
            return;
        };
        if deferred && capture.selection.agent_name.is_none() {
            return;
        }
        let Some(native) = self
            .starters
            .continuations
            .get_mut(&id)
            .and_then(|next| next.native.as_mut())
            .filter(|native| native.config.is_none())
        else {
            return;
        };
        let preferences = native.preferences.clone().unwrap_or_else(|| {
            state
                .config
                .iter()
                .map(|field| (field.id.clone(), field.value.clone()))
                .collect()
        });
        let (config, model) = if deferred {
            (
                crate::native_agent::CreationConfig {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
                    variant: None,
                    prepare_after_session: true,
                    correction_required: false,
                    config: preferences,
                },
                model.clone(),
            )
        } else {
            let Some(execution) = state.execution.as_ref() else {
                return;
            };
            (
                starter_native_config(execution, preferences),
                execution.model_choice(),
            )
        };
        if config.validate() && (deferred || self.starters.captures.bind_native_model(&id, model)) {
            native.config = Some(config);
            self.queue_persistence(cx);
        }
    }

    /// Native reads complete without a Window. Retire the activation immediately,
    /// then restore its full source through its original window after this borrow.
    pub(crate) fn fail_starter_native_preparation(
        &mut self,
        key: &openchamber_native_core::native_agent::OwnerKey,
        cx: &mut Context<Self>,
    ) {
        if key.scope.session_id.is_some() {
            return;
        }
        let Some(capture) = self.starters.captures.iter().find(|capture| {
            capture.owner.runtime().as_str() == key.server
                && capture.owner.directory() == key.scope.directory
                && capture
                    .selection
                    .model
                    .as_ref()
                    .is_some_and(|model| model.provider_id == key.scope.provider_id)
        }) else {
            return;
        };
        let id = capture.id.clone();
        let runtime = capture.owner.runtime().clone();
        let Some(next) = self.starters.continuations.remove(&id) else {
            return;
        };
        self.starters.capture_tasks.remove(&id);
        let entity = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = cx.update_window(next.window, |_, window, cx| {
                let _ = entity.update(cx, |this, cx| {
                    this.restore_starter_capture(&id, window, cx);
                    if this.runtime == runtime {
                        this.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                    }
                    cx.notify();
                });
            });
        });
    }

    /// Explicit recovery and late reads use the original complete draft owner.
    /// Conflicting metadata or identities keep their capture available to retry.
    pub(crate) fn restore_starter_capture(
        &mut self,
        id: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.persistence_loaded()
            || self.close_in_progress()
            || self.starters.capture_pending(id)
        {
            return false;
        }
        self.save_composer_draft(cx);
        let restored = self.starters.captures.restore_draft(id, &mut self.drafts);
        match restored {
            Ok(Some(capture)) => {
                capture.restore_selection(&mut self.composer_selections);
                if let Some(mode) = capture.permission_mode {
                    self.permission_modes
                        .drafts
                        .entry(capture.owner.runtime().clone())
                        .or_insert(mode);
                }
                if let Some(draft) = self.drafts.get(&capture.owner).cloned() {
                    self.save_draft_preview(&capture.owner, &draft, cx);
                }
                if self.composer_key.as_ref() == Some(&capture.owner) {
                    self.restore_composer_draft(window, cx);
                } else {
                    // A failed empty starter can be the last owner of a folder
                    // whose preparation finished while another view was open.
                    self.cleanup_unused_chat_directory(&capture.owner, window, cx);
                }
                self.queue_persistence(cx);
                true
            }
            Ok(None) => false,
            Err(_) => {
                self.notify_mutation_failure("chat.nativeRecovery.contentConflict", window, cx);
                self.queue_persistence(cx);
                false
            }
        }
    }

    /// Pins a built-in to the global list.
    pub(crate) fn add_starter(&mut self, starter: StarterRef, cx: &mut Context<Self>) {
        if let Some(next) = draft_starters::add(&self.global_starters(), starter) {
            self.save_global_starters(next, cx);
        }
    }

    /// Unpins a chip. A shared starter leaves only through the repository.
    pub(crate) fn remove_starter(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(chip) = self.chip(id) else {
            return;
        };
        match chip.group {
            StarterGroup::Global => {
                let next = draft_starters::remove(&self.global_starters(), &chip.starter);
                self.save_global_starters(next, cx);
            }
            StarterGroup::Project if !chip.shared => {
                if let Some(project) = self.ready_project_starters() {
                    let personal = draft_starters::personal(&project);
                    let next = draft_starters::remove(&personal, &chip.starter);
                    self.save_project_starters(next, None, cx);
                }
            }
            StarterGroup::Project => {}
        }
    }

    /// Moves a chip to where another of its group is.
    pub(crate) fn reorder_starter(&mut self, from: &str, to: &str, cx: &mut Context<Self>) {
        let (Some(from), Some(to)) = (self.chip(from), self.chip(to)) else {
            return;
        };
        if from.group != to.group {
            return;
        }
        match from.group {
            StarterGroup::Global => {
                if let Some(next) =
                    draft_starters::reorder(&self.global_starters(), &from.starter, &to.starter)
                {
                    self.save_global_starters(next, cx);
                }
            }
            // Shared chips keep their place in front.
            StarterGroup::Project => {
                if let Some(project) = self.ready_project_starters()
                    && let Some(next) = draft_starters::reorder(
                        &draft_starters::personal(&project),
                        &from.starter,
                        &to.starter,
                    )
                {
                    self.save_project_starters(next, None, cx);
                }
            }
        }
    }

    /// Moves a project chip into the repository's shared file, or back into
    /// the user's own list: the shared file first, then the personal list.
    pub(crate) fn toggle_shared_starter(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(chip) = self
            .chip(id)
            .filter(|chip| chip.group == StarterGroup::Project)
        else {
            return;
        };
        let Some(project) = self.ready_project_starters() else {
            return;
        };
        let shared = draft_starters::shared(&project);
        let personal = draft_starters::personal(&project);
        let (shared_next, personal_next) = if chip.shared {
            let mut personal = draft_starters::remove(&personal, &chip.starter);
            personal.push(chip.starter.clone());
            (draft_starters::remove(&shared, &chip.starter), personal)
        } else {
            let Some(shared) = draft_starters::add(&shared, chip.starter.clone()) else {
                return;
            };
            (shared, draft_starters::remove(&personal, &chip.starter))
        };
        self.save_project_starters(personal_next, Some(shared_next), cx);
    }

    fn ready_project_starters(&self) -> Option<Vec<ProjectStarter>> {
        match self
            .starter_project()
            .and_then(|path| self.starters.project.get(&path))
        {
            Some(ProjectStarters::Ready(list)) => Some(list.clone()),
            _ => None,
        }
    }

    /// Shows `next` at once and saves it with both offered-starter markers,
    /// one save at a time with the newest list.
    fn save_global_starters(&mut self, next: Vec<StarterRef>, cx: &mut Context<Self>) {
        self.starters.global_edit = Some(next.clone());
        if self.starters.global_saving {
            self.starters.global_pending = Some(next);
        } else {
            self.start_global_save(next, cx);
        }
        cx.notify();
    }

    fn start_global_save(&mut self, next: Vec<StarterRef>, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let changes = SettingsChanges {
            draft_starters: Some(next),
            draft_starters_craft_goal_added: Some(true),
            draft_starters_schedule_task_added: Some(true),
            ..SettingsChanges::default()
        };
        let save = connection
            .network
            .run(async move { client.save_settings(&changes).await });
        self.starters.global_saving = true;
        let generation = self.starters.generation;
        self.starters.task(|id| {
            cx.spawn(async move |this, cx| {
                let result = save.await;
                let _ = this.update(cx, |this, cx| {
                    this.starters.tasks.remove(&id);
                    if generation != this.starters.generation {
                        return;
                    }
                    this.starters.global_saving = false;
                    if let Some(pending) = this.starters.global_pending.take() {
                        this.start_global_save(pending, cx);
                        return;
                    }
                    match result {
                        Some(Ok(settings)) => {
                            if let Some(held) = this.send_catalog.settings.as_mut() {
                                held.draft_starters = settings.draft_starters.clone();
                            }
                            if settings.draft_starters == this.starters.global_edit {
                                this.starters.global_edit = None;
                            }
                        }
                        // The edit stays on screen, as the web keeps it.
                        failure => {
                            tracing::warn!(error = ?failure.and_then(Result::err), "the starters could not be saved");
                        }
                    }
                    cx.notify();
                });
            })
        });
    }

    /// Shows the user's own project list (and the shared one, when it
    /// changes too) at once and saves it: the shared file first, then the
    /// personal list. An edit made while a save is out waits as the newest
    /// pending one; the answer replaces the shown list only when none waits.
    fn save_project_starters(
        &mut self,
        personal: Vec<StarterRef>,
        shared: Option<Vec<StarterRef>>,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.starter_project() else {
            return;
        };
        let Some(ProjectStarters::Ready(list)) = self.starters.project.get_mut(&path) else {
            return;
        };
        let shared_now = shared
            .clone()
            .unwrap_or_else(|| draft_starters::shared(list));
        *list = shared_now
            .into_iter()
            .map(|starter| ProjectStarter {
                starter,
                source: StarterSource::Shared,
            })
            .chain(personal.iter().cloned().map(|starter| ProjectStarter {
                starter,
                source: StarterSource::Personal,
            }))
            .collect();
        if self.starters.project_saving.contains_key(&path) {
            let shared = shared
                .or_else(|| {
                    self.starters
                        .project_pending
                        .remove(&path)
                        .and_then(|(_, shared)| shared)
                })
                .or_else(|| self.starters.project_saving.get(&path).cloned().flatten());
            self.starters
                .project_pending
                .insert(path, (personal, shared));
        } else {
            self.start_project_save(path, personal, shared, cx);
        }
        cx.notify();
    }

    fn start_project_save(
        &mut self,
        path: String,
        personal: Vec<StarterRef>,
        shared: Option<Vec<StarterRef>>,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        self.starters
            .project_saving
            .insert(path.clone(), shared.clone());
        let save_path = path.clone();
        let save = connection.network.run(async move {
            if let Some(shared) = &shared {
                client.save_shared_starters(&save_path, shared).await?;
            }
            client.save_project_starters(&save_path, &personal).await
        });
        let generation = self.starters.generation;
        self.starters.task(|id| {
            cx.spawn(async move |this, cx| {
                let result = save.await;
                let _ = this.update(cx, |this, cx| {
                    this.starters.tasks.remove(&id);
                    if generation != this.starters.generation {
                        return;
                    }
                    this.starters.project_saving.remove(&path);
                    match result {
                        Some(Ok(list)) => {
                            match this.starters.project_pending.remove(&path) {
                                Some((personal, shared)) => {
                                    this.start_project_save(path, personal, shared, cx)
                                }
                                None => {
                                    this.starters.project.insert(path, ProjectStarters::Ready(list));
                                }
                            }
                        }
                        // Read again: the files say what was saved.
                        failure => {
                            tracing::warn!(error = ?failure.and_then(Result::err), "the project's starters could not be saved");
                            match this.starters.project_pending.remove(&path) {
                                Some((personal, shared)) => {
                                    this.start_project_save(path, personal, shared, cx);
                                }
                                None => {
                                    this.starters.project.remove(&path);
                                }
                            }
                        }
                    }
                    cx.notify();
                });
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_starter_renders_from_the_exported_templates() {
        let catalog = MagicCatalog::get();
        for starter in draft_starters::BUILTIN_STARTERS {
            let parsed = magic_prompt::parse_slash_command(starter.command).expect("a command");
            let (command, key) = catalog.command(&parsed.name).expect("a prompt pair");
            assert!(
                !command.requires_session,
                "{} works in a draft",
                starter.name
            );
            assert!(key.starts_with("chat.chatInput.toast."));
            let (visible, instructions) = catalog
                .render(command, "", &HashMap::new())
                .expect("built-in templates");
            assert!(!visible.is_empty() && !instructions.is_empty());
            assert!(
                !visible.contains("{{"),
                "{} leaves a placeholder",
                starter.name
            );
        }
        let (goal, _) = catalog.command("craft-goal").expect("a prompt pair");
        let (visible, _) = catalog
            .render(goal, "ship it", &HashMap::new())
            .expect("built-in templates");
        assert!(visible.ends_with("Here is my initial idea:\nship it"));
        let overrides = HashMap::from([(goal.visible.clone(), "mine {{idea_block}}".to_owned())]);
        let (visible, _) = catalog.render(goal, "", &overrides).expect("override");
        assert_eq!(visible, "mine ");
    }
}
