//! The session goal in the composer, as `SessionGoalButton`,
//! `SessionGoalRow`, `SessionGoalDialog` and the armed send of
//! `session-ui-store.ts` drive it.
//!
//! The server's goal loop owns every goal; this module only arms the next
//! send, writes the goal through `metadata.openchamber.goal` and reads what
//! the server reports back on the session record. The arm is memory only, as
//! the web's arm store is: a restart or a runtime switch leaves nothing armed,
//! while a goal already written comes back from the server's records. Every
//! write completes against the client and session it captured; its answer
//! lands only while that runtime is still current.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{AppContext, Context, Entity, Subscription, Task, Window};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::session::{MetadataFacts, SessionPatch};
use openchamber_native_core::session_goal::{
    self, DEFAULT_TOKEN_BUDGET, MAX_TOKEN_BUDGET, MIN_TOKEN_BUDGET, SessionGoal,
};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::prompt::Delivery;
use openchamber_native_transport::session_goal::{GoalInput, GoalStatusChange, new_goal_id};

use crate::connection::ConnectionState;
use crate::network::NetworkTask;
use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;

/// How long a small-model answer stands before it is read again.
const SMALL_MODEL_MAX_AGE: Duration = Duration::from_secs(60);

/// A goal the next prompt of a new session starts, written once the session
/// exists and before its prompt is admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingGoal {
    pub input: GoalInput,
    /// The first-turn reminder's message ID, minted before the prompt's.
    pub intro: MessageId,
}

/// The send that continues once its goal is saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalFollowUp {
    /// The composer alone, with its delivery; `now` sends over a working turn.
    Composer {
        delivery: Option<Delivery>,
        now: bool,
    },
    /// The composer with the queued messages ahead of it.
    WithQueue { delivery: Option<Delivery> },
}

/// The file-backed objective of one goal edit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ObjectiveKey {
    runtime: RuntimeId,
    session: SessionId,
    goal: String,
    updated_at: i64,
}

/// The create or manage dialog while it is open.
pub(crate) struct GoalDialog {
    pub key: SessionKey,
    pub objective: Entity<TextareaState>,
    pub budget: Entity<InputState>,
    pub budget_enabled: bool,
    /// The objective field was seeded from the goal or is the user's.
    pub seeded: bool,
    pub busy: bool,
    _subscriptions: Vec<Subscription>,
}

/// The goal state of the window.
#[derive(Default)]
pub(crate) struct Goals {
    /// The composer's arm: the next sent prompt becomes the objective.
    pub armed: bool,
    /// Sends to existing sessions waiting for their goal to be saved.
    pub arming: HashMap<SessionKey, Task<()>>,
    /// Goals of new sessions' first prompts, by the prompt's ID.
    pub created: HashMap<MessageId, PendingGoal>,
    /// Sessions with a status change in flight.
    pub busy: HashSet<SessionKey>,
    pub dialog: Option<GoalDialog>,
    objectives: HashMap<ObjectiveKey, Option<String>>,
    objective_reads: HashSet<ObjectiveKey>,
    small_model: HashMap<(RuntimeId, String), (Option<bool>, Instant)>,
    small_model_reads: HashSet<(RuntimeId, String)>,
    tasks: HashMap<u64, Task<()>>,
    next_task: u64,
}

impl Goals {
    fn spawn(&mut self, task: impl FnOnce(u64) -> Task<()>) {
        let id = self.next_task;
        self.next_task += 1;
        let task = task(id);
        self.tasks.insert(id, task);
    }

    /// Leaves a runtime: reads and the dialog belonged to it. The arm stays,
    /// as the web's arm store does not follow the runtime.
    pub(crate) fn switch_runtime(&mut self) {
        self.dialog = None;
        self.busy.clear();
        self.objective_reads.clear();
        self.small_model_reads.clear();
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl Workspace {
    /// Settings → Chat → Goal: absent shows the goal controls. Synthetic
    /// data has no server to run a goal.
    pub(crate) fn goals_enabled(&self) -> bool {
        !self.is_synthetic()
            && self
                .send_catalog
                .settings
                .as_ref()
                .and_then(|settings| settings.goal_enabled)
                .unwrap_or(true)
    }

    fn connected_client(&self) -> Option<(ServerClient, tokio::runtime::Handle)> {
        let connection = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)?;
        Some((connection.client.clone()?, connection.network.handle()))
    }

    /// The selected session's goal.
    pub(crate) fn selected_goal(&self) -> Option<&SessionGoal> {
        self.selected_session()?.goal.as_ref()
    }

    /// The directory the composer's goal controls read availability for.
    fn goal_directory(&self) -> Option<String> {
        match self.selected_session() {
            Some(session) => session.resolved_directory(),
            None => self
                .composer_key
                .as_ref()
                .map(|key| key.directory().to_owned()),
        }
    }

    /// `useGoalCheckAvailable`.
    pub(crate) fn goal_check_available(&self) -> bool {
        let checker = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.goal_checker)
            .unwrap_or_default();
        let small_model = self.goal_directory().and_then(|directory| {
            self.goals
                .small_model
                .get(&(self.runtime.clone(), directory))
                .and_then(|(available, _)| *available)
        });
        session_goal::check_available(checker, self.send_catalog.safety_net, small_model)
    }

    /// The objective text a goal shows: inline, or its file once read.
    pub(crate) fn goal_objective(&self, session: &SessionId, goal: &SessionGoal) -> Option<String> {
        if let Some(objective) = goal.inline_objective() {
            return Some(objective.to_owned());
        }
        self.goals
            .objectives
            .get(&ObjectiveKey {
                runtime: self.runtime.clone(),
                session: session.clone(),
                goal: goal.id.clone(),
                updated_at: goal.updated_at,
            })
            .cloned()
            .flatten()
    }

    /// Reads the selected goal's objective file, once per edit.
    pub(crate) fn ensure_goal_reads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.goals_enabled() {
            return;
        }
        let Some(session) = self.selected.clone() else {
            return;
        };
        let Some(goal) = self.selected_goal().filter(|goal| goal.objective_file) else {
            return;
        };
        let key = ObjectiveKey {
            runtime: self.runtime.clone(),
            session: session.clone(),
            goal: goal.id.clone(),
            updated_at: goal.updated_at,
        };
        if self.goals.objectives.contains_key(&key) || self.goals.objective_reads.contains(&key) {
            return;
        }
        let Some((client, executor)) = self.connected_client() else {
            return;
        };
        self.goals.objective_reads.insert(key.clone());
        let read =
            NetworkTask::spawn_on(
                &executor,
                async move { client.goal_objective(&session).await },
            );
        self.goals.spawn(|id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = read.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.goals.tasks.remove(&id);
                    this.goals.objective_reads.remove(&key);
                    if key.runtime != this.runtime {
                        return;
                    }
                    // Display only: a missing file or a failed read shows the
                    // generic goal title, as on the web.
                    let text = match result {
                        Some(Ok(text)) => text,
                        _ => None,
                    };
                    this.goals.objectives.insert(key, text);
                    this.seed_goal_dialog(window, cx);
                    cx.notify();
                });
            })
        });
    }

    /// Reads whether the small model can run for the composer's directory,
    /// at most once a minute. Read when a goal is about to start, so an
    /// idle composer costs no request; until it answers, a goal may start.
    fn refresh_small_model(&mut self, cx: &mut Context<Self>) {
        let Some((client, executor)) = self.connected_client() else {
            return;
        };
        let Some(directory) = self.goal_directory() else {
            return;
        };
        let small = (self.runtime.clone(), directory.clone());
        let fresh = self
            .goals
            .small_model
            .get(&small)
            .is_some_and(|(_, at)| at.elapsed() < SMALL_MODEL_MAX_AGE);
        if fresh || !self.goals.small_model_reads.insert(small.clone()) {
            return;
        }
        let read = NetworkTask::spawn_on(&executor, async move {
            client.small_model_available(Some(&directory)).await
        });
        self.goals.spawn(|id| {
            cx.spawn(async move |this, cx| {
                let result = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.goals.tasks.remove(&id);
                    this.goals.small_model_reads.remove(&small);
                    if small.0 != this.runtime {
                        return;
                    }
                    // A failed read keeps what was known: an unreachable
                    // server is no evidence the model went away.
                    let previous = this.goals.small_model.get(&small).and_then(|entry| entry.0);
                    let answer = match result {
                        Some(Ok(Some(available))) => Some(available),
                        _ => previous,
                    };
                    this.goals
                        .small_model
                        .insert(small, (answer, Instant::now()));
                    // Nothing can check a goal armed before the answer.
                    if this.selected_goal().is_none() && !this.goal_check_available() {
                        this.goals.armed = false;
                    }
                    cx.notify();
                });
            })
        });
    }

    /// The composer's target button: with a goal it opens the dialog; else it
    /// arms or disarms the next send, unless nothing can check a goal.
    pub(crate) fn press_goal_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_goal().is_some() {
            self.open_goal_dialog(window, cx);
            return;
        }
        if !self.goal_check_available() {
            return;
        }
        self.goals.armed = !self.goals.armed;
        if self.goals.armed {
            self.refresh_small_model(cx);
        }
        cx.notify();
    }

    /// Whether the selected session's send waits for its goal to be saved.
    pub(crate) fn goal_arming_selected(&self) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|session| self.goals.arming.contains_key(&self.mutation_key(session)))
    }

    /// The goal an armed send starts, consuming the arm: the composer's text
    /// as the objective, fitted for the audit, and the default budget.
    fn take_goal_arm(&mut self, text: &str) -> Option<GoalInput> {
        let objective = text.trim();
        if !self.goals.armed || !self.goals_enabled() || objective.is_empty() {
            return None;
        }
        self.goals.armed = false;
        Some(GoalInput {
            objective: session_goal::fit_objective(objective),
            token_budget: self
                .send_catalog
                .settings
                .as_ref()
                .and_then(|settings| settings.goal_default_budget),
        })
    }

    /// An armed send to an existing session: saves the goal and admits its
    /// first-turn reminder first, then sends the composer as it was. The
    /// composer is read-only meanwhile. When the goal cannot be saved the arm
    /// comes back and nothing is sent; when the send can no longer go out
    /// (another session or runtime is current), the goal is removed again.
    /// Returns whether this send was taken.
    pub(crate) fn send_with_goal(
        &mut self,
        follow_up: GoalFollowUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.selected.clone() else {
            return false;
        };
        let key = self.mutation_key(&session);
        if self.goals.arming.contains_key(&key) {
            return true;
        }
        let text = self.composer.read(cx).value().to_string();
        let Some((client, executor)) = self.connected_client() else {
            return false;
        };
        let Some(input) = self.take_goal_arm(&text) else {
            return false;
        };
        let owner = self.composer_key.clone();
        let intro = self.message_ids.next().0;
        let task_key = key.clone();
        let rollback_executor = executor.clone();
        // A send starts a new goal, whatever goal the session had.
        let request = NetworkTask::spawn_on(&executor, async move {
            let at = now_ms();
            let facts = client
                .set_session_goal(&session, &input, None, &new_goal_id(at), at)
                .await?;
            client
                .admit_goal_intro(&session, &intro, input.token_budget)
                .await?;
            Ok::<_, TransportError>((facts, client, session))
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.goals.arming.remove(&key);
                match result {
                    Some(Ok((facts, client, session))) => {
                        let current = key.runtime == this.runtime
                            && this.selected.as_ref() == Some(&session)
                            && this.composer_key == owner;
                        if key.runtime == this.runtime {
                            this.apply_goal_facts(&session, facts, window, cx);
                        }
                        if current {
                            match follow_up {
                                GoalFollowUp::Composer { delivery, now } => {
                                    this.submit_after_goal(delivery, now, window, cx)
                                }
                                GoalFollowUp::WithQueue { delivery } => {
                                    this.send_now(delivery, window, cx)
                                }
                            }
                        } else {
                            // The send cannot follow its goal: take it back.
                            this.goals.armed = true;
                            this.spawn_goal_rollback(client, &rollback_executor, session, cx);
                            this.notify_mutation_failure(
                                "chat.goal.toast.actionFailed",
                                window,
                                cx,
                            );
                        }
                    }
                    _ => {
                        this.goals.armed = true;
                        this.notify_mutation_failure("chat.goal.toast.actionFailed", window, cx);
                    }
                }
                cx.notify();
            });
        });
        self.goals.arming.insert(task_key, task);
        cx.notify();
        true
    }

    fn spawn_goal_rollback(
        &mut self,
        client: ServerClient,
        executor: &tokio::runtime::Handle,
        session: SessionId,
        cx: &mut Context<Self>,
    ) {
        let request = NetworkTask::spawn_on(executor, async move {
            client.clear_session_goal(&session).await
        });
        self.goals.spawn(|id| {
            cx.spawn(async move |this, cx| {
                let _ = request.await;
                let _ = this.update(cx, |this, _| {
                    this.goals.tasks.remove(&id);
                });
            })
        });
    }

    /// The goal of a new session's first prompt, consuming the arm. The
    /// reminder's ID is minted here, before the prompt's.
    pub(crate) fn take_created_goal(&mut self, text: &str) -> Option<PendingGoal> {
        let input = self.take_goal_arm(text)?;
        Some(PendingGoal {
            input,
            intro: self.message_ids.next().0,
        })
    }

    /// A first prompt whose goal could not be saved goes back to its draft,
    /// and the arm comes back with it.
    pub(crate) fn release_created_goal(&mut self, prompt: &MessageId, saved: bool) {
        if self.goals.created.remove(prompt).is_some() && !saved {
            self.goals.armed = true;
        }
    }

    /// A goal write's answer, applied as the server's metadata.
    pub(crate) fn apply_goal_facts(
        &mut self,
        session: &SessionId,
        facts: MetadataFacts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_server_events(
            vec![ServerEvent::SessionPatched {
                session_id: session.clone(),
                patch: SessionPatch {
                    metadata: Some(facts),
                    ..SessionPatch::default()
                },
            }],
            window,
            cx,
        );
    }

    /// Pause or Resume from the goal row.
    pub(crate) fn change_goal_status(
        &mut self,
        status: GoalStatusChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        if self.goals.busy.contains(&key) {
            return;
        }
        let Some((client, executor)) = self.connected_client() else {
            return;
        };
        let request = NetworkTask::spawn_on(&executor, async move {
            let facts = client
                .set_session_goal_status(&session, status, now_ms())
                .await;
            (session, facts)
        });
        self.goals.busy.insert(key.clone());
        self.goals.spawn(|id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.goals.tasks.remove(&id);
                    this.goals.busy.remove(&key);
                    if key.runtime != this.runtime {
                        return;
                    }
                    match result {
                        Some((session, Ok(facts))) => {
                            this.apply_goal_facts(&session, facts, window, cx)
                        }
                        _ => {
                            this.notify_mutation_failure("chat.goal.toast.actionFailed", window, cx)
                        }
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    /// Opens the create or manage dialog for the selected session.
    pub(crate) fn open_goal_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        let goal = self.selected_goal().cloned();
        if goal.is_none() {
            self.refresh_small_model(cx);
        }
        let seeded = goal
            .as_ref()
            .and_then(|goal| self.goal_objective(&session, goal));
        let placeholder = crate::locale::strings(cx).get("chat.goal.dialog.objectivePlaceholder");
        let objective = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .auto_grow(4, 8)
                .placeholder(placeholder);
            if let Some(text) = &seeded {
                state.set_value(text.clone(), window, cx);
            }
            state
        });
        let budget_value = goal
            .as_ref()
            .and_then(|goal| goal.token_budget)
            .unwrap_or(DEFAULT_TOKEN_BUDGET);
        let budget =
            cx.new(|cx| InputState::new(window, cx).default_value(budget_value.to_string()));
        let subscriptions = vec![
            cx.subscribe_in(&objective, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(dialog) = &mut this.goals.dialog
                {
                    dialog.seeded = true;
                    cx.notify();
                }
            }),
            cx.subscribe_in(&budget, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        self.goals.dialog = Some(GoalDialog {
            key,
            objective,
            budget,
            budget_enabled: goal
                .as_ref()
                .is_some_and(|goal| goal.token_budget.is_some()),
            seeded: seeded.is_some() || !goal.as_ref().is_some_and(|goal| goal.objective_file),
            busy: false,
            _subscriptions: subscriptions,
        });
        self.show_goal_dialog(window, cx);
    }

    /// A file-backed objective that arrives after the dialog opened fills
    /// the field while it is still untouched.
    fn seed_goal_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.goals.dialog.as_ref().filter(|dialog| !dialog.seeded) else {
            return;
        };
        let session = dialog.key.session.clone();
        let field = dialog.objective.clone();
        let Some(text) = self
            .sessions
            .get(&session)
            .and_then(|record| record.goal.as_ref())
            .and_then(|goal| self.goal_objective(&session, goal))
        else {
            return;
        };
        field.update(cx, |field, cx| {
            if field.value().is_empty() {
                field.set_value(text, window, cx);
            }
        });
        if let Some(dialog) = &mut self.goals.dialog {
            dialog.seeded = true;
        }
    }

    /// The dialog's budget as entered: `NumberInput` keeps a positive whole
    /// number in range and falls back to its minimum otherwise.
    pub(crate) fn goal_dialog_budget(&self, cx: &gpui_kit::App) -> Option<u64> {
        let dialog = self.goals.dialog.as_ref()?;
        if !dialog.budget_enabled {
            return None;
        }
        let value = dialog
            .budget
            .read(cx)
            .value()
            .trim()
            .replace([',', '_', ' '], "")
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value > 0.0)
            .map_or(MIN_TOKEN_BUDGET, |value| value.floor() as u64);
        Some(value.clamp(MIN_TOKEN_BUDGET, MAX_TOKEN_BUDGET))
    }

    /// Whether Save or Start may run (`canSave`).
    pub(crate) fn goal_dialog_can_save(&self, cx: &gpui_kit::App) -> bool {
        let Some(dialog) = &self.goals.dialog else {
            return false;
        };
        let goal = self
            .sessions
            .get(&dialog.key.session)
            .and_then(|record| record.goal.as_ref());
        let objective = dialog.objective.read(cx).value().trim().to_owned();
        if objective.is_empty()
            || dialog.busy
            || goal.is_some_and(|goal| !goal.is_live())
            || (goal.is_none() && !self.goal_check_available())
        {
            return false;
        }
        let Some(goal) = goal else {
            return true;
        };
        let saved = self
            .goal_objective(&dialog.key.session, goal)
            .unwrap_or_default();
        objective != saved || self.goal_dialog_budget(cx) != goal.token_budget
    }

    pub(crate) fn toggle_goal_dialog_budget(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.goals.dialog {
            dialog.budget_enabled = !dialog.budget_enabled;
            cx.notify();
        }
    }

    /// Start or Save: a new goal, or the edit of the live one.
    pub(crate) fn save_goal_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.goal_dialog_can_save(cx) {
            return;
        }
        let Some(dialog) = &self.goals.dialog else {
            return;
        };
        let key = dialog.key.clone();
        let input = GoalInput {
            objective: session_goal::fit_objective(dialog.objective.read(cx).value().trim()),
            token_budget: self.goal_dialog_budget(cx),
        };
        let existing = self
            .sessions
            .get(&key.session)
            .and_then(|record| record.goal.clone());
        self.goal_dialog_write(
            key,
            move |client: ServerClient, session: SessionId| async move {
                let at = now_ms();
                client
                    .set_session_goal(&session, &input, existing.as_ref(), &new_goal_id(at), at)
                    .await
            },
            window,
            cx,
        );
    }

    /// Remove goal.
    pub(crate) fn clear_goal_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.goals.dialog else {
            return;
        };
        if dialog.busy {
            return;
        }
        let key = dialog.key.clone();
        self.goal_dialog_write(
            key,
            |client: ServerClient, session: SessionId| async move {
                client.clear_session_goal(&session).await
            },
            window,
            cx,
        );
    }

    /// One write from the dialog; the dialog closes once it lands and stays
    /// open with a notice when it fails.
    fn goal_dialog_write<F, Fut>(
        &mut self,
        key: SessionKey,
        write: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(ServerClient, SessionId) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<MetadataFacts, TransportError>> + Send + 'static,
    {
        let Some((client, executor)) = self.connected_client() else {
            return;
        };
        let session = key.session.clone();
        let request = NetworkTask::spawn_on(&executor, async move { write(client, session).await });
        if let Some(dialog) = &mut self.goals.dialog {
            dialog.busy = true;
        }
        self.goals.spawn(|id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.goals.tasks.remove(&id);
                    let open = this
                        .goals
                        .dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.key == key);
                    if let Some(dialog) = this.goals.dialog.as_mut().filter(|_| open) {
                        dialog.busy = false;
                    }
                    if key.runtime != this.runtime {
                        return;
                    }
                    match result {
                        Some(Ok(facts)) => {
                            this.apply_goal_facts(&key.session, facts, window, cx);
                            if open {
                                this.close_goal_dialog(window, cx);
                            }
                        }
                        _ => {
                            this.notify_mutation_failure("chat.goal.toast.actionFailed", window, cx)
                        }
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    pub(crate) fn close_goal_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.goals.dialog = None;
        gpui_kit::component::WindowExt::close_dialog(window, cx);
        cx.notify();
    }
}

/// The goal step of a created session's first prompt, run in its send chain
/// before anything is admitted: the goal, then its first-turn reminder.
pub(crate) async fn save_created_goal(
    client: &ServerClient,
    session: &SessionId,
    goal: &PendingGoal,
) -> Result<MetadataFacts, TransportError> {
    let at = now_ms();
    let facts = client
        .set_session_goal(session, &goal.input, None, &new_goal_id(at), at)
        .await?;
    client
        .admit_goal_intro(session, &goal.intro, goal.input.token_budget)
        .await?;
    Ok(facts)
}
