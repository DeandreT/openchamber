//! The starter chips of the new-session view (`DraftPresetChips.tsx` with
//! `useDraftStarters.ts`): which chips show, what a chip submits, and the
//! edits that pin, remove, reorder and share them.
//!
//! A chip submits at once: the starter, then the composer's text, creates
//! the selected project's session as Enter would. A built-in starter is one
//! of the composer's prompt-pair commands; it renders its visible prompt and
//! its instructions from the overrides the server resolves for the project,
//! read for each activation, over the built-in templates the resource
//! generator exports. A failed read sends nothing.

use std::collections::HashMap;
use std::sync::OnceLock;

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::draft_starters::{
    self, Available, ProjectStarter, ResolvedStarter, StarterGroup, StarterRef, StarterSource,
};
use openchamber_native_core::magic_prompt::{
    self, CommandVariable, MagicCommand, MagicPrompts, PromptSide,
};
use openchamber_native_transport::client::SettingsChanges;
use openchamber_native_transport::prompt_parts::{PromptParts, SyntheticContext};
use serde::Deserialize;

use crate::locale;
use crate::workspace::Workspace;

const MAGIC_PROMPTS_JSON: &str = include_str!("../resources/magic-prompts.json");

/// A starter's first prompt for a new session.
pub(crate) struct StarterPrompt {
    /// What the chip submitted, the composer's text included
    /// (`presetText`): what a definite failure gives back to the composer.
    pub draft_text: String,
    /// The rendered prompt the session receives.
    pub prompt_text: String,
    pub parts: PromptParts,
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
    /// A chip whose overrides read is in flight; others wait.
    activating: bool,
    /// The persisted draft remains owned here until creation takes it.
    captured: Option<(DraftKey, Draft)>,
    generation: u64,
    tasks: HashMap<u64, Task<()>>,
    next_task: u64,
}

impl Starters {
    pub(crate) fn persisted_draft(&self, owner: &DraftKey, current: Draft) -> Draft {
        match &self.captured {
            Some((key, captured)) if key == owner => {
                if current.text.is_empty() {
                    captured.clone()
                } else if captured.text.is_empty() {
                    current
                } else {
                    Draft::new(format!("{}\n\n{}", current.text, captured.text))
                }
            }
            _ => current,
        }
    }

    pub(crate) fn visible_draft(&self, owner: &DraftKey, stored: Draft) -> Draft {
        let Some((key, captured)) = &self.captured else {
            return stored;
        };
        if key != owner || captured.text.is_empty() {
            return stored;
        }
        if stored.text == captured.text {
            return Draft::default();
        }
        let suffix = format!("\n\n{}", captured.text);
        match stored.text.strip_suffix(&suffix) {
            Some(text) => Draft::new(text),
            None => stored,
        }
    }

    /// Forgets everything of the previous runtime.
    pub(crate) fn reset(&mut self) {
        *self = Self {
            generation: self.generation + 1,
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
        if self.starters.activating || !self.can_create_connected() {
            return;
        }
        let Some(chip) = self.chip(id) else {
            return;
        };
        let Some(directory) = self.starter_project() else {
            return;
        };
        let Some(source) = self.composer_key.clone() else {
            return;
        };
        let typed = self.composer.read(cx).value().to_string();
        let text = draft_starters::submission(&chip, &typed);
        let catalog = MagicCatalog::get();
        let Some((command, error_key)) = magic_prompt::parse_slash_command(&text)
            .and_then(|parsed| catalog.command(&parsed.name).map(|found| (parsed, found)))
            .filter(|(_, (command, _))| !command.requires_session)
            .map(|(parsed, (command, key))| ((command.clone(), parsed.argument), key.to_owned()))
        else {
            // Only built-ins resolve here, and each is a prompt pair.
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let read_directory = directory.clone();
        let read = connection
            .network
            .run(async move { client.magic_prompt_overrides(Some(&read_directory)).await });
        self.starters.activating = true;
        self.save_composer_draft(cx);
        self.starters.captured = Some((source.clone(), Draft::new(typed.clone())));
        // The text the chip carries leaves the composer now; typing while the
        // prompts load stays, and a failure gives the carried text back.
        if !typed.is_empty() {
            self.composer
                .update(cx, |composer, cx| composer.set_value("", window, cx));
            self.queue_persistence(cx);
        }
        let generation = self.starters.generation;
        self.starters.task(|task_id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = read.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.starters.tasks.remove(&task_id);
                    if generation != this.starters.generation {
                        // The captured draft remains in storage after reset.
                        return;
                    }
                    this.starters.activating = false;
                    this.starters.captured = None;
                    let (command, argument) = command;
                    let rendered = match result {
                        Some(Ok(overrides)) => catalog.render(&command, &argument, &overrides),
                        failure => {
                            tracing::warn!(error = ?failure.and_then(Result::err), "the magic prompts could not be read");
                            None
                        }
                    };
                    // The view may have moved on while the overrides were read.
                    let Some((visible, instructions)) = rendered
                        .filter(|_| this.starter_project().as_deref() == Some(directory.as_str()))
                        .filter(|_| this.composer_key.as_ref() == Some(&source))
                    else {
                        if this.composer_key.as_ref() == Some(&source) {
                            this.give_back_text(&source, typed, window, cx);
                        }
                        window.push_notification(
                            Notification::error(locale::strings(cx).get(&error_key)),
                            cx,
                        );
                        cx.notify();
                        return;
                    };
                    let (id, _) = this.message_ids.next();
                    let parts = PromptParts {
                        context: vec![SyntheticContext {
                            id,
                            text: instructions,
                            description: None,
                            metadata: None,
                        }],
                        ..PromptParts::default()
                    };
                    let created = this.submit_new_session_with(
                        Some(StarterPrompt {
                            draft_text: text,
                            prompt_text: visible,
                            parts,
                        }),
                        window,
                        cx,
                    );
                    if !created {
                        this.give_back_text(&source, typed, window, cx);
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
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
