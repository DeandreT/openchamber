//! Settings → Magic Prompts (`MagicPromptsSidebar.tsx`, `MagicPromptsPage.tsx`):
//! the prompt groups, and one group's editors in the global or the project
//! scope. Each prompt saves explicitly. Drafts, the last read and the write in
//! flight are kept per context (one scope on one runtime), so nothing typed or
//! answered in one context shows in another.
//!
//! What blocks closing Settings is derived: a draft whose last save failed and
//! that still differs from what its context resolves to. A failed reset leaves
//! no edit behind and blocks nothing.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Toggled, WeakEntity,
    Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::magic_prompts::{
    MagicPromptError, MagicPromptOverrides, MagicPromptScope, MagicPromptScopeState,
    MagicPromptView,
};
use serde::Deserialize;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    ServerTarget, SettingsWrites, focus_handle, helper, info_hint, same_target,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

const DEFINITIONS_JSON: &str = include_str!("../resources/magic-prompt-settings.json");

/// One built-in prompt: what Settings says about it and its built-in text.
#[derive(Deserialize)]
struct Definition {
    description: String,
    placeholders: Vec<String>,
    template: String,
}

#[derive(Deserialize)]
struct DefinitionsResource {
    definitions: HashMap<String, Definition>,
}

fn definition(id: &str) -> &'static Definition {
    static DEFINITIONS: OnceLock<HashMap<String, Definition>> = OnceLock::new();
    DEFINITIONS
        .get_or_init(|| {
            let resource: DefinitionsResource = serde_json::from_str(DEFINITIONS_JSON)
                .expect("resources/magic-prompt-settings.json is generated JSON");
            resource.definitions
        })
        .get(id)
        .unwrap_or_else(|| panic!("magic prompt {id} is not exported for Settings"))
}

/// Which prompts a group edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blocks {
    /// The visible prompt and its instructions.
    Pair,
    /// Instructions alone (`walkthrough`).
    InstructionsOnly,
    /// A pair and the code fusion instructions (`session.fusion`).
    Fusion,
}

/// One sidebar entry (`PROMPT_PAGE_MAP` and the sidebar's groups). `name` is
/// the suffix its locale keys share.
struct Group {
    id: &'static str,
    name: &'static str,
    section: &'static str,
    blocks: Blocks,
}

const fn group(
    id: &'static str,
    name: &'static str,
    section: &'static str,
    blocks: Blocks,
) -> Group {
    Group {
        id,
        name,
        section,
        blocks,
    }
}

/// The sidebar, in its order.
#[rustfmt::skip]
const GROUPS: [Group; 18] = [
    group("git.commit.generate", "gitCommitGenerate", "git", Blocks::Pair),
    group("git.pr.generate", "gitPrGenerate", "git", Blocks::Pair),
    group("walkthrough", "walkthrough", "git", Blocks::InstructionsOnly),
    group("git.conflict.resolve", "gitConflictResolve", "git", Blocks::Pair),
    group("git.integrate.cherrypick.resolve", "gitCherrypickConflictResolve", "git", Blocks::Pair),
    group("plan.todo", "planTodo", "planning", Blocks::Pair),
    group("plan.improve", "planImprove", "planning", Blocks::Pair),
    group("plan.implement", "planImplement", "planning", Blocks::Pair),
    group("session.explore", "sessionExplore", "session", Blocks::Pair),
    group("session.summary", "sessionSummary", "session", Blocks::Pair),
    group("session.review", "sessionWorkspaceReview", "session", Blocks::Pair),
    group("session.plan", "sessionFeaturePlan", "session", Blocks::Pair),
    group("session.craftGoal", "sessionCraftGoal", "session", Blocks::Pair),
    group("session.catchup", "sessionCatchUp", "session", Blocks::Pair),
    group("session.debug", "sessionDebug", "session", Blocks::Pair),
    group("session.weigh", "sessionWeigh", "session", Blocks::Pair),
    group("session.fusion", "sessionFusion", "session", Blocks::Fusion),
    group("memory.tidy", "memoryTidy", "memory", Blocks::Pair),
];

/// One editor of a group: the prompt it edits and its title.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Block {
    id: String,
    title: &'static str,
}

impl Block {
    fn is_visible_prompt(&self) -> bool {
        self.id.ends_with(".visible")
    }
}

impl Group {
    fn blocks(&self) -> Vec<Block> {
        let block = |suffix: &str, title: &'static str| Block {
            id: format!("{}.{suffix}", self.id),
            title,
        };
        let visible = block("visible", "settings.magicPrompts.page.block.visiblePrompt");
        let instructions = block(
            "instructions",
            "settings.magicPrompts.page.block.instructions",
        );
        match self.blocks {
            Blocks::Pair => vec![visible, instructions],
            Blocks::InstructionsOnly => vec![instructions],
            Blocks::Fusion => vec![
                visible,
                instructions,
                block(
                    "codeInstructions",
                    "settings.magicPrompts.page.block.codeFusionInstructions",
                ),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Global,
    Project,
}

/// The registered project the project scope edits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectTarget {
    id: String,
    path: String,
}

/// One scope on one runtime.
type ContextKey = (RuntimeId, MagicPromptScope);

/// What a context last read. A failed read keeps its reason and is never an
/// empty view.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ContextLoad {
    Loading,
    Ready(MagicPromptView),
    Failed(String),
}

/// The one write a context may have in flight. Each answer is a whole view of
/// the scope, so a context runs its writes one at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ContextWrite {
    Save(String),
    Reset(String),
    ResetAll,
}

struct ContextState {
    load: ContextLoad,
    drafts: HashMap<String, String>,
    write: Option<ContextWrite>,
    /// Prompts whose last save failed. One counts only while its draft still
    /// differs from the baseline (`save_still_failed`).
    failed_saves: HashSet<String>,
    /// The latest read; an older answer is dropped.
    reads: u64,
    /// Writes started; a read that began before one is dropped.
    mutations: u64,
    read: Option<Task<()>>,
}

impl Default for ContextState {
    fn default() -> Self {
        Self {
            load: ContextLoad::Loading,
            drafts: HashMap::new(),
            write: None,
            failed_saves: HashSet::new(),
            reads: 0,
            mutations: 0,
            read: None,
        }
    }
}

impl ContextState {
    /// Whether prompt `id` holds an edit its last save did not write: the
    /// save failed and the draft still differs from what the context resolves
    /// to. A view that is not readable cannot show the draft as saved.
    fn save_still_failed(&self, id: &str) -> bool {
        if !self.failed_saves.contains(id) {
            return false;
        }
        let Some(draft) = self.drafts.get(id) else {
            return false;
        };
        match &self.load {
            ContextLoad::Ready(view) => match editable_of(view) {
                Editable::Editable {
                    project, global, ..
                } => draft != &resolve(project.as_ref(), &global, id).0,
                Editable::Blocked { .. } => true,
            },
            ContextLoad::Loading | ContextLoad::Failed(_) => true,
        }
    }
}

/// Where a prompt's shown text comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Project,
    Global,
    Builtin,
}

/// Why the project scope cannot be edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blocked {
    ProjectInvalid,
    GlobalInvalid,
}

/// A ready view, as the editors use it (`editableOf`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Editable {
    Editable {
        /// The overrides of the scope being edited.
        own: MagicPromptOverrides,
        /// The project layer; `None` in the global scope.
        project: Option<MagicPromptOverrides>,
        global: MagicPromptOverrides,
    },
    Blocked {
        error: Blocked,
        reason: String,
        path: String,
    },
}

fn editable_of(view: &MagicPromptView) -> Editable {
    match view {
        MagicPromptView::Global(global) => Editable::Editable {
            own: global.clone(),
            project: None,
            global: global.clone(),
        },
        MagicPromptView::Project(view) => {
            let path = format!("{}/{}", view.path, view.file);
            match (&view.project, &view.global) {
                (MagicPromptScopeState::Invalid { reason }, _) => Editable::Blocked {
                    error: Blocked::ProjectInvalid,
                    reason: reason.clone(),
                    path,
                },
                // Without the global layer the inherited text is unknown, and
                // the built-in text in its place would be wrong.
                (_, MagicPromptScopeState::Invalid { reason }) => Editable::Blocked {
                    error: Blocked::GlobalInvalid,
                    reason: reason.clone(),
                    path,
                },
                (
                    MagicPromptScopeState::Readable(project),
                    MagicPromptScopeState::Readable(global),
                ) => Editable::Editable {
                    own: project.clone(),
                    project: Some(project.clone()),
                    global: global.clone(),
                },
            }
        }
    }
}

/// Project, then global, then built-in. Presence decides, so an empty
/// override still wins (`resolveMagicPromptTemplate`).
fn resolve(
    project: Option<&MagicPromptOverrides>,
    global: &MagicPromptOverrides,
    id: &str,
) -> (String, Source) {
    if let Some(text) = project.and_then(|project| project.get(id)) {
        return (text.clone(), Source::Project);
    }
    if let Some(text) = global.get(id) {
        return (text.clone(), Source::Global);
    }
    (definition(id).template.clone(), Source::Builtin)
}

struct BlockEditor {
    input: Entity<TextareaState>,
    _subscription: Subscription,
}

pub(crate) struct MagicPromptsPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    project: Option<ProjectTarget>,
    scope: Scope,
    /// The selected group's ID.
    selected: &'static str,
    contexts: HashMap<ContextKey, ContextState>,
    editors: HashMap<String, BlockEditor>,
    writes: SettingsWrites,
    /// Whether Settings shows this page. A hidden page reads nothing.
    shown: bool,
    /// Feedback of the last action, kept for tests.
    notice: Option<SharedString>,
    /// A search result's control, focused once its editor exists.
    pending_reveal: Option<&'static str>,
    list_scroll: ScrollHandle,
    scroll: ScrollHandle,
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl MagicPromptsPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                let retargeted = !same_target(this.target.as_ref(), target.as_ref());
                if this.shown {
                    if retargeted || this.project != project(&this.owner, cx) {
                        this.refresh(cx);
                    }
                } else if retargeted && this.target.is_some() {
                    // Hidden: the retired connection is let go without a
                    // read. Showing the page adopts the current one.
                    this.target = None;
                    cx.notify();
                }
            }));
        }
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            project: None,
            scope: Scope::Global,
            selected: GROUPS[0].id,
            contexts: HashMap::new(),
            editors: HashMap::new(),
            writes: SettingsWrites::default(),
            shown: false,
            notice: None,
            pending_reveal: None,
            list_scroll: ScrollHandle::new(),
            scroll: ScrollHandle::new(),
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Settings shows or leaves this page. Showing it reads again.
    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.shown = shown;
        if shown {
            self.refresh(cx);
        }
    }

    /// Adopts the current server and project, then reads the open context
    /// again so edits made elsewhere show.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        if !same_target(self.target.as_ref(), target.as_ref()) {
            // A retired connection's reads and writes never land: completions
            // compare their captured target with this one.
            for context in self.contexts.values_mut() {
                context.read = None;
                context.write = None;
            }
        }
        self.target = target;
        self.project = project(&self.owner, cx);
        self.load_context(cx);
        cx.notify();
    }

    fn context_key(&self) -> Option<ContextKey> {
        let runtime = self.target.as_ref()?.runtime.clone();
        Some(match self.scope {
            Scope::Global => (runtime, MagicPromptScope::Global),
            Scope::Project => (
                runtime,
                MagicPromptScope::Project(self.project.as_ref()?.id.clone()),
            ),
        })
    }

    fn context(&self) -> Option<&ContextState> {
        self.contexts.get(&self.context_key()?)
    }

    /// Reads the open context. A context read before keeps showing while it
    /// is read again.
    fn load_context(&mut self, cx: &mut Context<Self>) {
        let (Some(key), Some(target)) = (self.context_key(), self.target.clone()) else {
            return;
        };
        let context = self.contexts.entry(key.clone()).or_default();
        context.reads += 1;
        let read = context.reads;
        let mutations = context.mutations;
        if !matches!(context.load, ContextLoad::Ready(_)) {
            context.load = ContextLoad::Loading;
        }
        let client = target.client.clone();
        let scope = key.1.clone();
        let reading = NetworkTask::spawn_on(&target.network, async move {
            client.magic_prompt_view(&scope).await
        });
        context.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if !same_target(this.target.as_ref(), Some(&target)) {
                    return;
                }
                let Some(context) = this.contexts.get_mut(&key) else {
                    return;
                };
                if context.reads != read || context.mutations != mutations {
                    return;
                }
                context.read = None;
                context.load = match result {
                    Some(Ok(view)) => ContextLoad::Ready(view),
                    Some(Err(error)) => ContextLoad::Failed(error.to_string()),
                    None => ContextLoad::Failed(String::new()),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope == scope || (scope == Scope::Project && self.project.is_none()) {
            return;
        }
        self.scope = scope;
        self.load_context(cx);
        cx.notify();
    }

    fn select(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.selected != id {
            self.selected = id;
            self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
            cx.notify();
        }
    }

    fn selected_group(&self) -> &'static Group {
        GROUPS
            .iter()
            .find(|group| group.id == self.selected)
            .unwrap_or(&GROUPS[0])
    }

    fn set_draft(&mut self, id: &str, value: String, cx: &mut Context<Self>) {
        let Some(key) = self.context_key() else {
            return;
        };
        if let Some(context) = self.contexts.get_mut(&key) {
            context.drafts.insert(id.to_owned(), value);
            cx.notify();
        }
    }

    /// The text a block shows: its draft, else the resolved template.
    fn shown(&self, id: &str) -> Option<String> {
        let context = self.context()?;
        let ContextLoad::Ready(view) = &context.load else {
            return None;
        };
        let Editable::Editable {
            project, global, ..
        } = editable_of(view)
        else {
            return None;
        };
        Some(
            context
                .drafts
                .get(id)
                .cloned()
                .unwrap_or_else(|| resolve(project.as_ref(), &global, id).0),
        )
    }

    /// Runs one write of the open context, or nothing while that context
    /// already has one in flight. Its answer replaces the context's view. After
    /// a failure the server state is unknown, so the context is read again;
    /// the draft stays and Save sends it again.
    fn run_write(&mut self, write: ContextWrite, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(key), Some(target)) = (self.context_key(), self.target.clone()) else {
            return;
        };
        let Some(context) = self.contexts.get_mut(&key) else {
            return;
        };
        if context.write.is_some() {
            return;
        }
        // What success may clear: a draft still holding this text.
        let expected: Vec<(String, String)> = match &write {
            ContextWrite::Save(id) | ContextWrite::Reset(id) => context
                .drafts
                .get(id)
                .map(|draft| (id.clone(), draft.clone()))
                .into_iter()
                .collect(),
            ContextWrite::ResetAll => context
                .drafts
                .iter()
                .map(|(id, draft)| (id.clone(), draft.clone()))
                .collect(),
        };
        context.write = Some(write.clone());
        context.mutations += 1;
        let client = target.client.clone();
        let scope = key.1.clone();
        let request = write.clone();
        let text = match &write {
            ContextWrite::Save(id) => expected
                .first()
                .map(|(_, draft)| draft.clone())
                .unwrap_or_else(|| self.shown(id).unwrap_or_default()),
            _ => String::new(),
        };
        let (id, writing) = self.writes.submit(&target, async move {
            match &request {
                // A global text equal to the built-in one needs no override.
                // A project one is kept: it still wins over a global override.
                ContextWrite::Save(prompt)
                    if scope == MagicPromptScope::Global && text == definition(prompt).template =>
                {
                    client.reset_magic_prompt(&scope, prompt).await
                }
                ContextWrite::Save(prompt) => client.save_magic_prompt(&scope, prompt, &text).await,
                ContextWrite::Reset(prompt) => client.reset_magic_prompt(&scope, prompt).await,
                ContextWrite::ResetAll => client.reset_magic_prompts(&scope).await,
            }
        });
        let project_scope = matches!(key.1, MagicPromptScope::Project(_));
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<MagicPromptView, MagicPromptError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                let live = same_target(this.target.as_ref(), Some(&target))
                    && same_target(
                        ServerTarget::capture(&this.owner, this.network.as_ref(), cx).as_ref(),
                        Some(&target),
                    );
                if !live {
                    // The retired connection's answer is dropped; `refresh`
                    // already released its context.
                    return;
                }
                let Some(context) = this.contexts.get_mut(&key) else {
                    return;
                };
                context.write = None;
                let strings = locale::strings(cx).clone();
                match result {
                    Some(Ok(view)) => {
                        context.load = ContextLoad::Ready(view);
                        for (prompt, draft) in &expected {
                            if context.drafts.get(prompt) == Some(draft) {
                                context.drafts.remove(prompt);
                            }
                        }
                        if let ContextWrite::Save(prompt) = &write {
                            context.failed_saves.remove(prompt);
                        }
                        let key = match (&write, project_scope) {
                            (ContextWrite::Save(_), _) => "settings.magicPrompts.page.toast.saved",
                            (ContextWrite::Reset(_), true) => {
                                "settings.magicPrompts.page.toast.projectOverrideRemoved"
                            }
                            (ContextWrite::Reset(_), false) => {
                                "settings.magicPrompts.page.toast.resetSuccess"
                            }
                            (ContextWrite::ResetAll, true) => {
                                "settings.magicPrompts.page.toast.projectOverridesRemoved"
                            }
                            (ContextWrite::ResetAll, false) => {
                                "settings.magicPrompts.page.toast.resetAllSuccess"
                            }
                        };
                        let text = strings.get(key);
                        this.notice = Some(text.clone());
                        window.push_notification(Notification::success(text), cx);
                    }
                    failure => {
                        // Only a save leaves an edit unwritten; a failed
                        // reset changes nothing the user typed.
                        if let ContextWrite::Save(prompt) = &write {
                            context.failed_saves.insert(prompt.clone());
                        }
                        let title = strings.get(match write {
                            ContextWrite::Save(_) => "settings.magicPrompts.page.toast.saveFailed",
                            ContextWrite::Reset(_) => {
                                "settings.magicPrompts.page.toast.resetFailed"
                            }
                            ContextWrite::ResetAll => {
                                "settings.magicPrompts.page.toast.resetAllFailed"
                            }
                        });
                        let reason = failure
                            .and_then(Result::err)
                            .map(|error| error.to_string())
                            .unwrap_or_default();
                        this.notice = Some(title.clone());
                        window.push_notification(Notification::error(reason).title(title), cx);
                        this.load_context(cx);
                    }
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    fn save_prompt(&mut self, block: &Block, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.shown(&block.id) else {
            return;
        };
        if block.is_visible_prompt() && text.trim().is_empty() {
            let text =
                locale::strings(cx).get("settings.magicPrompts.page.toast.visiblePromptRequired");
            self.notice = Some(text.clone());
            window.push_notification(Notification::error(text), cx);
            return;
        }
        self.run_write(ContextWrite::Save(block.id.clone()), window, cx);
    }

    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let pending = self.writes.take_pending_write();
        cx.notify();
        pending
    }

    /// Whether a draft of the current runtime still holds an edit its last
    /// save did not write.
    pub(crate) fn has_failed_writes(&self) -> bool {
        let Some(runtime) = self.target.as_ref().map(|target| &target.runtime) else {
            return false;
        };
        self.contexts.iter().any(|((owner, _), context)| {
            owner == runtime
                && context
                    .failed_saves
                    .iter()
                    .any(|prompt| context.save_still_failed(prompt))
        })
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    /// Opens a Settings search result: the scope choice and Reset all sit at
    /// the top; a block result focuses the first matching editor.
    pub(crate) fn reveal(&mut self, item: &'static str, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
        if matches!(
            item,
            "magic-prompts.visible-prompt" | "magic-prompts.instructions"
        ) {
            self.pending_reveal = Some(item);
        }
        cx.notify();
    }
}

/// The registered project that owns the workspace's current directory: the
/// directory itself, or the project of the worktree it is. The reference's
/// Settings project selector starts on the chat's project the same way.
fn project(owner: &WeakEntity<Workspace>, cx: &App) -> Option<ProjectTarget> {
    let owner = owner.upgrade()?;
    let workspace = owner.read(cx);
    let directory = workspace.config_directory()?;
    let owning = workspace.owning_project(&directory)?;
    workspace
        .topology
        .projects
        .iter()
        .find(|project| project.path == owning)
        .map(|project| ProjectTarget {
            id: project.id.as_str().to_owned(),
            path: project.path.clone(),
        })
}

impl MagicPromptsPage {
    /// Creates the selected group's editors and gives each the text its
    /// context shows. Typing reports a change; a value set here does not, so
    /// switching contexts never writes a draft.
    fn sync_editors(&mut self, blocks: &[Block], window: &mut Window, cx: &mut Context<Self>) {
        let mono_rows = (9, 40);
        for block in blocks {
            let Some(shown) = self.shown(&block.id) else {
                continue;
            };
            if !self.editors.contains_key(&block.id) {
                let input =
                    cx.new(|cx| TextareaState::new(window, cx).auto_grow(mono_rows.0, mono_rows.1));
                let id = block.id.clone();
                let subscription =
                    cx.subscribe_in(&input, window, move |this, input, event, _, cx| {
                        if matches!(event, InputEvent::Change) {
                            let value = input.read(cx).value().to_string();
                            this.set_draft(&id, value, cx);
                        }
                    });
                self.editors.insert(
                    block.id.clone(),
                    BlockEditor {
                        input,
                        _subscription: subscription,
                    },
                );
            }
            let input = self.editors[&block.id].input.clone();
            if input.read(cx).value().as_ref() != shown.as_str() {
                input.update(cx, |input, cx| input.set_value(shown, window, cx));
            }
        }
        if let Some(item) = self.pending_reveal {
            let wanted = blocks.iter().find(|block| {
                block.is_visible_prompt() == (item == "magic-prompts.visible-prompt")
            });
            if let Some(editor) = wanted.and_then(|block| self.editors.get(&block.id)) {
                let input = editor.input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
                self.pending_reveal = None;
            }
        }
    }

    fn button(
        &mut self,
        id: SharedString,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        on_activate(
            gpui_kit::base::Button::new(id.clone())
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(!enabled)
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(label),
            cx,
            action,
        )
        .into_any_element()
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let mut list = div()
            .id("magic-prompts-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (index, group) in GROUPS.iter().enumerate() {
            if index == 0 || GROUPS[index - 1].section != group.section {
                list = list.child(
                    div()
                        .px(px(4.0))
                        .pt(px(if index == 0 { 0.0 } else { 10.0 }))
                        .pb(px(2.0))
                        .css_text(0.6875, 1.0, cx)
                        .text_color(palette.muted_foreground)
                        .child(strings.get(&format!(
                            "settings.magicPrompts.sidebar.group.{}",
                            group.section
                        ))),
                );
            }
            let selected = self.selected == group.id;
            let id: SharedString =
                format!("magic-prompts-row-{}", group.id.replace('.', "-")).into();
            let focus = focus_handle(&mut self.focus, &id, cx);
            let label = strings.get(&format!(
                "settings.magicPrompts.sidebar.item.{}",
                group.name
            ));
            let group_id = group.id;
            let hover = palette.hover;
            list = list.child(
                on_activate(
                    div()
                        .id(id.clone())
                        .accessibility_id(id)
                        .role(Role::Button)
                        .aria_label(label.clone())
                        .aria_selected(selected)
                        .track_focus(&focus)
                        .tab_index(0)
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .truncate()
                        .when(selected, |row| row.bg(palette.selection))
                        .when(!selected, |row| row.hover(move |style| style.bg(hover)))
                        .child(label),
                    cx,
                    move |this, _, cx| this.select(group_id, cx),
                )
                .test_support(),
            );
        }
        div()
            .id("magic-prompts-sidebar")
            .w(px(256.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_color(palette.border)
            .when(direction == Direction::LeftToRight, |side| {
                side.border_r_1()
            })
            .when(direction == Direction::RightToLeft, |side| {
                side.border_l_1()
            })
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(16.0))
                    .pb(px(12.0))
                    .border_b_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .id("magic-prompts-sidebar-title")
                            .role(Role::Heading)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(strings.get("settings.magicPrompts.sidebar.title")),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .css_text(0.8125, 1.25, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.get("settings.magicPrompts.sidebar.description")),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn scope_chip(&mut self, scope: Scope, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let (id, label): (SharedString, _) = match scope {
            Scope::Global => (
                "magic-prompts-scope-global".into(),
                strings.get("settings.common.scope.global"),
            ),
            Scope::Project => (
                "magic-prompts-scope-project".into(),
                strings.get("settings.common.scope.project"),
            ),
        };
        let selected = self.scope == scope;
        let disabled = scope == Scope::Project && self.project.is_none() && !selected;
        let focus = focus_handle(&mut self.focus, &id, cx);
        let hover = palette.hover;
        let chip = div()
            .id(id.clone())
            .accessibility_id(id)
            .role(Role::Button)
            .aria_label(label.clone())
            .aria_toggled(if selected {
                Toggled::True
            } else {
                Toggled::False
            })
            // A disabled chip takes no focus or activation.
            .when(!disabled, |chip| chip.track_focus(&focus).tab_index(0))
            .h_full()
            .px(px(10.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .when(selected, |chip| {
                chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
            })
            .when(disabled, |chip| {
                chip.opacity(0.5).text_color(palette.muted_foreground)
            })
            .when(!disabled && !selected, |chip| {
                chip.cursor_pointer().hover(move |style| style.bg(hover))
            })
            .child(label);
        if disabled {
            return chip.into_any_element();
        }
        on_activate(chip, cx, move |this, _, cx| this.set_scope(scope, cx))
            .test_support()
            .into_any_element()
    }

    fn scope_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let global = self.scope_chip(Scope::Global, cx);
        let project = self.scope_chip(Scope::Project, cx);
        // One line under the choice: what the scope covers, the file a project
        // override lives in, or why there is no project scope.
        let hint = match (&self.project, self.scope) {
            (None, _) => helper(
                strings.get("settings.magicPrompts.page.scope.noProject"),
                cx,
            ),
            (Some(_), Scope::Global) => helper(
                strings.get("settings.magicPrompts.page.scope.globalHint"),
                cx,
            ),
            (Some(project), Scope::Project) => {
                let file = match self.context().map(|context| &context.load) {
                    Some(ContextLoad::Ready(MagicPromptView::Project(view))) => {
                        format!("{}/{}", view.path, view.file)
                    }
                    _ => project.path.clone(),
                };
                let name = project
                    .path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                div()
                    .id("magic-prompts-scope-file")
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .min_w_0()
                    .css_text(0.8125, 1.25, cx)
                    .text_color(palette.muted_foreground)
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(palette.foreground)
                            .child(name),
                    )
                    .child("·")
                    .child(
                        div()
                            .flex_shrink_0()
                            .child(strings.get("settings.magicPrompts.page.scope.savedInLabel")),
                    )
                    // A path reads left to right in every language.
                    .child(div().min_w_0().truncate().child(file))
                    .into_any_element()
            }
        };
        div()
            .id("magic-prompts-scope")
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(strings.get("settings.magicPrompts.page.scope.label")),
                    )
                    .child(info_hint(
                        "magic-prompts.scope",
                        strings.get("settings.magicPrompts.page.scope.info"),
                        cx,
                    )),
            )
            .child(
                div().flex_row_from(direction).child(
                    div()
                        .id("magic-prompts-scope-group")
                        .role(Role::Group)
                        .aria_label(strings.get("settings.magicPrompts.page.scope.label"))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(2.0))
                        .p(px(2.0))
                        .h(gpui_kit::rems(2.0))
                        .rounded(px(7.0))
                        .bg(palette.elevated)
                        .child(global)
                        .child(project),
                ),
            )
            .child(hint)
            .into_any_element()
    }

    /// A failed or blocked context: the reason, the file when it names one,
    /// and Retry. Never an empty editor.
    fn error(
        &mut self,
        message: SharedString,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let retry = self.button(
            "magic-prompts-load-failed-retry".into(),
            strings.get("settings.magicPrompts.page.actions.retry"),
            true,
            cx,
            |this, _, cx| this.load_context(cx),
        );
        div()
            .id("magic-prompts-load-failed")
            .accessibility_id("magic-prompts-load-failed")
            .role(Role::Alert)
            .aria_label(message.clone())
            .mt(px(24.0))
            .flex_row_from(strings.direction())
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .rounded(px(10.0))
            .px(px(16.0))
            .py(px(12.0))
            .bg(palette.elevated)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .css_text(0.8125, 1.25, cx)
                    .text_color(palette.status_error)
                    .child(message)
                    .children(path),
            )
            .child(retry)
            .into_any_element()
    }

    fn block(
        &mut self,
        block: &Block,
        own: &MagicPromptOverrides,
        project: Option<&MagicPromptOverrides>,
        global: &MagicPromptOverrides,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let definition = definition(&block.id);
        let (baseline, source) = resolve(project, global, &block.id);
        let context = self.context().expect("an editable context is open");
        let draft = context.drafts.get(&block.id).cloned();
        let write = context.write.clone();
        let dirty = draft.as_ref().is_some_and(|draft| draft != &baseline);
        let shown = draft.unwrap_or(baseline);
        let save_failed = context.save_still_failed(&block.id);
        let empty_visible = block.is_visible_prompt() && shown.trim().is_empty();
        let overridden = own.contains_key(&block.id);
        let saving = write == Some(ContextWrite::Save(block.id.clone()));
        let resetting = write == Some(ContextWrite::Reset(block.id.clone()));
        // Every write of this context waits for the one in flight.
        let busy = write.is_some() || self.writes.closing();
        let in_project = self.scope == Scope::Project;
        let status = strings.get(if dirty {
            "settings.magicPrompts.page.status.unsavedChanges"
        } else {
            match (source, in_project) {
                (Source::Project, _) => "settings.magicPrompts.page.status.usingProjectOverride",
                (Source::Global, true) => "settings.magicPrompts.page.status.inheritedFromGlobal",
                (Source::Global, false) => "settings.magicPrompts.page.status.usingGlobalOverride",
                (Source::Builtin, _) => "settings.magicPrompts.page.status.usingBuiltinDefault",
            }
        });
        let slug = block.id.replace('.', "-");
        let reset_label = strings.get(if resetting {
            "settings.magicPrompts.page.actions.resetting"
        } else if in_project {
            "settings.magicPrompts.page.actions.removeProjectOverride"
        } else {
            "settings.magicPrompts.page.actions.resetToDefault"
        });
        let reset_block = block.id.clone();
        let reset = self.button(
            format!("magic-prompts-reset-{slug}").into(),
            reset_label,
            overridden && !busy,
            cx,
            move |this, window, cx| {
                this.run_write(ContextWrite::Reset(reset_block.clone()), window, cx)
            },
        );
        let save_label = strings.get(if saving {
            "settings.common.actions.saving"
        } else {
            "settings.magicPrompts.page.actions.save"
        });
        let save_block = block.clone();
        let save = self.button(
            format!("magic-prompts-save-{slug}").into(),
            save_label,
            dirty && !busy && !empty_visible,
            cx,
            move |this, window, cx| this.save_prompt(&save_block, window, cx),
        );
        // A save that failed stays visible on its block until the draft is
        // saved or no longer differs.
        let failure = save_failed.then(|| {
            let text = strings.get("settings.magicPrompts.page.toast.saveFailed");
            let retry_block = block.clone();
            let retry = self.button(
                format!("magic-prompts-save-failed-{slug}-retry").into(),
                strings.get("settings.magicPrompts.page.actions.retry"),
                !busy && !empty_visible,
                cx,
                move |this, window, cx| this.save_prompt(&retry_block, window, cx),
            );
            let id: SharedString = format!("magic-prompts-save-failed-{slug}").into();
            div()
                .id(id.clone())
                .accessibility_id(id)
                .role(Role::Status)
                .aria_label(text.clone())
                .flex_row_from(direction)
                .flex_wrap()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .css_text(0.8125, 1.25, cx)
                        .text_color(palette.status_error)
                        .child(text),
                )
                .child(retry)
        });
        let placeholders = (!definition.placeholders.is_empty()).then(|| {
            let list: Vec<String> = definition
                .placeholders
                .iter()
                .map(|key| format!("{{{{{key}}}}}"))
                .collect();
            format!(
                "{} {}",
                strings.get("settings.magicPrompts.page.placeholdersLabel"),
                list.join(", ")
            )
        });
        let title = strings.get(block.title);
        let editor = self
            .editors
            .get(&block.id)
            .map(|editor| editor.input.clone());
        let mono = Theme::global(cx).mono_font_family.clone();
        div()
            .id(SharedString::from(format!("magic-prompts-block-{slug}")))
            .mt(px(32.0))
            .pt(px(24.0))
            .border_t_1()
            .border_color(palette.border)
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id(SharedString::from(format!("magic-prompts-title-{slug}")))
                                    .role(Role::Heading)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title.clone()),
                            )
                            .child(info_hint(
                                &format!("magic-prompts-{slug}"),
                                definition.description.clone().into(),
                                cx,
                            )),
                    )
                    // Placeholders are syntax the user reads while typing.
                    .children(placeholders.map(|text| helper(text.into(), cx))),
            )
            .children(editor.map(|editor| {
                div().css_text(0.875, 1.25, cx).font_family(mono).child(
                    Textarea::new(&editor)
                        .accessibility_id(SharedString::from(format!("magic-prompts-input-{slug}")))
                        .aria_label(title),
                )
            }))
            .children(failure)
            .children(empty_visible.then(|| {
                div()
                    .css_text(0.6875, 1.0, cx)
                    .text_color(palette.status_error)
                    .child(
                        strings.get("settings.magicPrompts.page.validation.visiblePromptRequired"),
                    )
            }))
            .child(
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id(SharedString::from(format!("magic-prompts-status-{slug}")))
                            .accessibility_id(SharedString::from(format!(
                                "magic-prompts-status-{slug}"
                            )))
                            .role(Role::Status)
                            .aria_label(status.clone())
                            .css_text(0.6875, 1.0, cx)
                            .text_color(palette.muted_foreground)
                            .child(status),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .child(reset)
                            .child(save),
                    ),
            )
            .into_any_element()
    }

    fn body(&mut self, blocks: &[Block], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        // The project scope without a project shows the scope choice alone.
        let Some(load) = self.context().map(|context| context.load.clone()) else {
            return Vec::new();
        };
        match load {
            ContextLoad::Loading => vec![
                div()
                    .id("magic-prompts-loading")
                    .accessibility_id("magic-prompts-loading")
                    .role(Role::Status)
                    .aria_label(strings.get("settings.magicPrompts.page.loading.aria"))
                    .mt(px(24.0))
                    .child(helper(
                        strings.get("settings.magicPrompts.page.loading.text"),
                        cx,
                    ))
                    .into_any_element(),
            ],
            ContextLoad::Failed(reason) => vec![self.error(
                strings.format(
                    "settings.magicPrompts.page.error.loadFailed",
                    &[("reason", &reason)],
                ),
                None,
                cx,
            )],
            ContextLoad::Ready(view) => match editable_of(&view) {
                Editable::Blocked {
                    error,
                    reason,
                    path,
                } => {
                    let (key, path) = match error {
                        Blocked::ProjectInvalid => (
                            "settings.magicPrompts.page.error.projectInvalid",
                            Some(path),
                        ),
                        Blocked::GlobalInvalid => {
                            ("settings.magicPrompts.page.error.globalInvalid", None)
                        }
                    };
                    vec![self.error(strings.format(key, &[("reason", &reason)]), path, cx)]
                }
                Editable::Editable {
                    own,
                    project,
                    global,
                } => blocks
                    .iter()
                    .map(|block| self.block(block, &own, project.as_ref(), &global, cx))
                    .collect(),
            },
        }
    }

    fn editor_view(&mut self, blocks: &[Block], cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let group = self.selected_group();
        let title = strings.get(&format!(
            "settings.magicPrompts.page.group.{}.title",
            group.name
        ));
        let description = strings.get(&format!(
            "settings.magicPrompts.page.group.{}.description",
            group.name
        ));
        let context = self.context();
        let write = context.and_then(|context| context.write.clone());
        let has_overrides = context.is_some_and(|context| match &context.load {
            ContextLoad::Ready(view) => match editable_of(view) {
                Editable::Editable { own, .. } => !own.is_empty(),
                Editable::Blocked { .. } => false,
            },
            _ => false,
        });
        let reset_all_label = strings.get(if write == Some(ContextWrite::ResetAll) {
            "settings.magicPrompts.page.actions.resetting"
        } else if self.scope == Scope::Project {
            "settings.magicPrompts.page.actions.resetAllProjectOverrides"
        } else {
            "settings.magicPrompts.page.actions.resetAllOverrides"
        });
        let reset_all = self.button(
            "magic-prompts-reset-all".into(),
            reset_all_label,
            has_overrides && write.is_none() && !self.writes.closing(),
            cx,
            |this, window, cx| this.run_write(ContextWrite::ResetAll, window, cx),
        );
        let scope = self.scope_section(cx);
        let body = self.body(blocks, cx);
        div()
            .css_text(0.875, 1.25, cx)
            .id("magic-prompts-editor")
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .when(direction == Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(
                div()
                    .mb(px(24.0))
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id("magic-prompts-title")
                                    .accessibility_id("magic-prompts-title")
                                    .role(Role::Heading)
                                    .aria_label(title.clone())
                                    .css_text(1.125, 1.5, cx)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(info_hint("magic-prompts-group", description, cx)),
                    )
                    .child(reset_all),
            )
            .child(scope)
            .children(body)
            .into_any_element()
    }
}

impl Render for MagicPromptsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        if self.target.is_none() {
            return div()
                .id("magic-prompts-settings-page")
                .size_full()
                .px(px(48.0))
                .py(px(32.0))
                .child(crate::settings_view::page_header(Page::MagicPrompts, cx))
                .child(helper(strings.get("common.unavailable"), cx))
                .into_any_element();
        }
        let blocks = self.selected_group().blocks();
        self.sync_editors(&blocks, window, cx);
        let sidebar = self.sidebar(cx);
        let editor = self.editor_view(&blocks, cx);
        div()
            .id("magic-prompts-settings-page")
            .role(Role::Group)
            .aria_label(strings.get(Page::MagicPrompts.title()))
            .size_full()
            .flex_row_from(strings.direction())
            .text_color(palette.foreground)
            .child(sidebar)
            .child(div().flex_1().min_w_0().h_full().child(editor))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, TestAppContext};
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::settings_controls::tests::{ResponseGate, retarget};
    use crate::settings_test_server::{Server, open, settings, settle};

    const VISIBLE: &str = "git.commit.generate.visible";
    const INSTRUCTIONS: &str = "git.commit.generate.instructions";

    /// A global override store. `fail_reads` and `fail_writes` answer 500
    /// with a reason while set.
    struct Fixture {
        server: Server,
        store: Arc<Mutex<Map<String, Value>>>,
        fail_reads: Arc<AtomicBool>,
        fail_writes: Arc<AtomicBool>,
    }

    fn fixture(initial: &[(&str, &str)]) -> Fixture {
        let store: Map<String, Value> = initial
            .iter()
            .map(|(key, value)| ((*key).to_owned(), json!(value)))
            .collect();
        let store = Arc::new(Mutex::new(store));
        let fail_reads = Arc::new(AtomicBool::new(false));
        let fail_writes = Arc::new(AtomicBool::new(false));
        let (held, reads, writes) = (
            Arc::clone(&store),
            Arc::clone(&fail_reads),
            Arc::clone(&fail_writes),
        );
        let server = Server::new(move |request| {
            let path = request.target.split('?').next().unwrap_or_default();
            if !path.starts_with("/api/magic-prompts") {
                return (404, json!({"error": "not in this fixture"}));
            }
            let id = path.strip_prefix("/api/magic-prompts/").map(str::to_owned);
            let mut store = held.lock().expect("store");
            if request.method == "GET" {
                if reads.load(Ordering::Acquire) {
                    return (500, json!({"error": "fixture read failure"}));
                }
            } else if writes.load(Ordering::Acquire) {
                return (500, json!({"error": "fixture write failure"}));
            } else {
                match (request.method.as_str(), id) {
                    ("PUT", Some(id)) => {
                        store.insert(id, request.body["text"].clone());
                    }
                    ("DELETE", Some(id)) => {
                        store.remove(&id);
                    }
                    ("DELETE", None) => store.clear(),
                    _ => return (404, json!({"error": "not in this fixture"})),
                }
            }
            (
                200,
                json!({"version": 1, "overrides": Value::Object(store.clone())}),
            )
        });
        Fixture {
            server,
            store,
            fail_reads,
            fail_writes,
        }
    }

    fn page(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<MagicPromptsPage> {
        let settings = settings(workspace, cx);
        cx.read(|cx| settings.read(cx).magic_prompts.clone())
    }

    fn act(
        window: AnyWindowHandle,
        page: &Entity<MagicPromptsPage>,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut MagicPromptsPage, &mut Window, &mut Context<MagicPromptsPage>),
    ) {
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| action(page, window, cx));
        })
        .expect("test window");
        settle(window, cx);
    }

    fn block(id: &str) -> Block {
        Block {
            id: id.to_owned(),
            title: "settings.magicPrompts.page.block.visiblePrompt",
        }
    }

    fn load(page: &Entity<MagicPromptsPage>, cx: &TestAppContext) -> Option<ContextLoad> {
        cx.read(|cx| page.read(cx).context().map(|context| context.load.clone()))
    }

    #[test]
    fn every_listed_prompt_has_an_exported_definition() {
        for group in &GROUPS {
            for block in group.blocks() {
                assert!(
                    !definition(&block.id).description.is_empty(),
                    "{} is described",
                    block.id
                );
            }
        }
    }

    #[test]
    fn an_invalid_scope_blocks_the_project_editors() {
        use openchamber_native_transport::magic_prompts::MagicPromptProjectView;
        let readable = || MagicPromptScopeState::Readable(MagicPromptOverrides::new());
        let invalid = || MagicPromptScopeState::Invalid {
            reason: "bad JSON".into(),
        };
        let view = |project, global| {
            MagicPromptView::Project(MagicPromptProjectView {
                path: "/work/app".into(),
                file: ".openchamber/project.json".into(),
                global,
                project,
            })
        };
        assert!(matches!(
            editable_of(&view(invalid(), readable())),
            Editable::Blocked {
                error: Blocked::ProjectInvalid,
                ref path,
                ..
            } if path == "/work/app/.openchamber/project.json"
        ));
        assert!(
            matches!(
                editable_of(&view(readable(), invalid())),
                Editable::Blocked {
                    error: Blocked::GlobalInvalid,
                    ..
                }
            ),
            "inherited text is unknown without the global layer"
        );
        let mut project = MagicPromptOverrides::new();
        project.insert(VISIBLE.into(), String::new());
        let mut global = MagicPromptOverrides::new();
        global.insert(VISIBLE.into(), "Global".into());
        global.insert(INSTRUCTIONS.into(), "Global rules".into());
        assert_eq!(
            resolve(Some(&project), &global, VISIBLE),
            (String::new(), Source::Project),
            "an empty project override still wins"
        );
        assert_eq!(
            resolve(Some(&project), &global, INSTRUCTIONS),
            ("Global rules".into(), Source::Global)
        );
        assert_eq!(
            resolve(None, &MagicPromptOverrides::new(), VISIBLE).1,
            Source::Builtin
        );
    }

    #[gpui_kit::test]
    fn the_global_scope_loads_and_shows_an_override_over_the_builtin_text(cx: &mut TestAppContext) {
        let fixture = fixture(&[(VISIBLE, "My visible prompt")]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        assert!(matches!(load(&page, cx), Some(ContextLoad::Ready(_))));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.shown(VISIBLE).as_deref(), Some("My visible prompt"));
            assert_eq!(
                page.shown(INSTRUCTIONS).as_deref(),
                Some(definition(INSTRUCTIONS).template.as_str()),
                "a prompt without an override shows the built-in text"
            );
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("magic-prompts-row-session-fusion")
                    .is_some()
            );
            assert!(
                window
                    .try_find("magic-prompts-save-git-commit-generate-visible")
                    .is_some()
            );
            assert!(window.try_find("magic-prompts-load-failed").is_none());
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_failed_read_shows_its_reason_and_retry_reads_again(cx: &mut TestAppContext) {
        let fixture = fixture(&[]);
        fixture.fail_reads.store(true, Ordering::Release);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        assert_eq!(
            load(&page, cx),
            Some(ContextLoad::Failed("fixture read failure".into()))
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("magic-prompts-load-failed-retry").is_some());
            assert!(
                window
                    .try_find("magic-prompts-save-git-commit-generate-visible")
                    .is_none(),
                "a failed read never renders editors over built-in text"
            );
        })
        .expect("test window");
        fixture.fail_reads.store(false, Ordering::Release);
        act(window, &page, cx, |page, _, cx| page.load_context(cx));
        assert!(matches!(load(&page, cx), Some(ContextLoad::Ready(_))));
    }

    #[gpui_kit::test]
    fn a_saved_edit_writes_the_override_and_the_builtin_text_removes_it(cx: &mut TestAppContext) {
        let fixture = fixture(&[]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.set_draft(VISIBLE, "Write a subject".into(), cx);
            page.save_prompt(&block(VISIBLE), window, cx);
        });
        let writes = fixture.server.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].is("PUT", "/api/magic-prompts/git.commit.generate.visible"));
        assert_eq!(writes[0].body, json!({"text": "Write a subject"}));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.notice.as_deref(), Some("Magic prompt saved"));
            assert!(
                page.context().expect("context").drafts.is_empty(),
                "the saved draft gives way to the answered view"
            );
            assert_eq!(page.shown(VISIBLE).as_deref(), Some("Write a subject"));
            assert!(!page.has_failed_writes());
        });
        let builtin = definition(VISIBLE).template.clone();
        act(window, &page, cx, |page, window, cx| {
            page.set_draft(VISIBLE, builtin.clone(), cx);
            page.save_prompt(&block(VISIBLE), window, cx);
        });
        assert!(
            fixture.server.writes()[1]
                .is("DELETE", "/api/magic-prompts/git.commit.generate.visible"),
            "the built-in text needs no global override"
        );
        assert!(fixture.store.lock().expect("store").is_empty());
    }

    #[gpui_kit::test]
    fn an_empty_visible_prompt_is_refused_before_any_request(cx: &mut TestAppContext) {
        let fixture = fixture(&[]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.set_draft(VISIBLE, "  ".into(), cx);
            page.save_prompt(&block(VISIBLE), window, cx);
        });
        assert!(fixture.server.writes().is_empty());
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("Visible prompt cannot be empty")
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_save_keeps_the_draft_and_the_same_save_retries(cx: &mut TestAppContext) {
        let fixture = fixture(&[]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.set_draft(INSTRUCTIONS, "Kept rules".into(), cx);
            page.save_prompt(&block(INSTRUCTIONS), window, cx);
        });
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(page.has_failed_writes());
            assert_eq!(page.shown(INSTRUCTIONS).as_deref(), Some("Kept rules"));
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("magic-prompts-save-failed-git-commit-generate-instructions-retry")
                    .is_some(),
                "the failed save shows on its block with Retry"
            );
        })
        .expect("test window");
        cx.read(|cx| {
            let page = page.read(cx);
            let context = page.context().expect("context");
            assert_eq!(context.write, None, "the context accepts the retry");
            assert!(
                matches!(context.load, ContextLoad::Ready(_)),
                "the view read before the failure keeps showing"
            );
        });
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.save_prompt(&block(INSTRUCTIONS), window, cx);
        });
        assert_eq!(
            fixture.store.lock().expect("store")[INSTRUCTIONS],
            "Kept rules"
        );
        cx.read(|cx| assert!(!page.read(cx).has_failed_writes()));
    }

    #[gpui_kit::test]
    fn the_close_block_follows_the_failed_draft_and_resets_never_set_it(cx: &mut TestAppContext) {
        let fixture = fixture(&[(VISIBLE, "Stored")]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.run_write(ContextWrite::Reset(VISIBLE.into()), window, cx);
        });
        act(window, &page, cx, |page, window, cx| {
            page.run_write(ContextWrite::ResetAll, window, cx);
        });
        cx.read(|cx| {
            assert!(
                !page.read(cx).has_failed_writes(),
                "a failed reset leaves no unsaved edit"
            );
        });
        act(window, &page, cx, |page, window, cx| {
            page.set_draft(VISIBLE, "Edited".into(), cx);
            page.save_prompt(&block(VISIBLE), window, cx);
        });
        cx.read(|cx| assert!(page.read(cx).has_failed_writes()));
        // Typing the stored text back leaves nothing to save.
        act(window, &page, cx, |page, _, cx| {
            page.set_draft(VISIBLE, "Stored".into(), cx);
        });
        cx.read(|cx| {
            assert!(
                !page.read(cx).has_failed_writes(),
                "a draft equal to its baseline no longer blocks closing"
            );
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("magic-prompts-save-failed-git-commit-generate-visible")
                    .is_none()
            );
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_worktree_directory_edits_its_owning_project(cx: &mut TestAppContext) {
        use openchamber_native_core::ownership::{ProjectWorktrees, WorktreeInfo};
        let fixture = fixture(&[]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        let worktree = "/work/linked-worktree";
        let owner = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_new_session(window, cx);
                    let owner = workspace.topology.projects[0].clone();
                    workspace.topology.worktrees.push(ProjectWorktrees {
                        project_path: owner.path.clone(),
                        worktrees: vec![WorktreeInfo {
                            path: worktree.into(),
                            ..WorktreeInfo::default()
                        }],
                    });
                    workspace
                        .draft_projects
                        .insert(workspace.runtime.clone(), worktree.into());
                    cx.notify();
                    owner
                })
            })
            .expect("test window");
        settle(window, cx);
        cx.read(|cx| {
            assert_eq!(
                workspace.read(cx).config_directory().as_deref(),
                Some(worktree),
                "the session works in the worktree"
            );
            assert_eq!(
                page.read(cx).project,
                Some(ProjectTarget {
                    id: owner.id.as_str().to_owned(),
                    path: owner.path.clone(),
                }),
                "the project scope edits the project that owns the worktree"
            );
        });
    }

    #[gpui_kit::test]
    fn a_hidden_page_reads_nothing_when_the_connection_changes(cx: &mut TestAppContext) {
        let first = fixture(&[]);
        let second = fixture(&[(VISIBLE, "From the new server")]);
        let (window, workspace) = open(&first.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, _, cx| page.set_shown(false, cx));
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        assert!(
            !second
                .server
                .requests()
                .iter()
                .any(|request| request.target.starts_with("/api/magic-prompts")),
            "a hidden page waits for its next visit"
        );
        act(window, &page, cx, |page, _, cx| page.set_shown(true, cx));
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).shown(VISIBLE).as_deref(),
                Some("From the new server")
            );
        });
    }

    #[gpui_kit::test]
    fn reset_all_clears_the_scope_and_one_write_runs_at_a_time(cx: &mut TestAppContext) {
        let fixture = fixture(&[(VISIBLE, "A"), (INSTRUCTIONS, "B")]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.run_write(ContextWrite::ResetAll, window, cx);
                // A second click before the answer finds the context busy.
                page.run_write(ContextWrite::Reset(VISIBLE.into()), window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        let writes = fixture.server.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].is("DELETE", "/api/magic-prompts"));
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("All prompt overrides reset")
            );
        });
    }

    #[gpui_kit::test]
    fn a_runtime_switch_drops_the_retired_read_and_reads_the_new_server(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let first = Server::new(move |_| {
            held.hold();
            (
                200,
                json!({"version": 1, "overrides": {VISIBLE: "From the retired server"}}),
            )
        });
        let second = fixture(&[(VISIBLE, "From the new server")]);
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                use openchamber_native_transport::client::{ServerClient, ServerSettings};
                use openchamber_native_transport::credential::Credential;

                use crate::connection::{Connection, ServerConfig};
                let mut connection =
                    Connection::new(ServerConfig::for_endpoint(first.endpoint.clone()))
                        .expect("network");
                connection.client = Some(
                    ServerClient::new(first.endpoint.clone(), Credential::None)
                        .expect("fixture client"),
                );
                workspace.connection = Some(connection);
                workspace.send_catalog.settings = Some(ServerSettings::default());
                workspace.show_settings(Some(Page::MagicPrompts), window, cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        let page = page(&workspace, cx);
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        gate.release();
        settle(window, cx);
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).shown(VISIBLE).as_deref(),
                Some("From the new server"),
                "the retired server's answer never replaces the current view"
            );
        });
    }

    #[gpui_kit::test]
    fn the_project_scope_needs_a_registered_project(cx: &mut TestAppContext) {
        let fixture = fixture(&[]);
        let (window, workspace) = open(&fixture.server, Page::MagicPrompts, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, _, cx| {
            page.project = None;
            page.set_scope(Scope::Project, cx);
            assert_eq!(page.scope, Scope::Global, "no project, no project scope");
        });
        assert!(
            !fixture
                .server
                .requests()
                .iter()
                .any(|request| request.target.contains("projectId")),
            "nothing is read for a project that is not registered"
        );
    }
}
