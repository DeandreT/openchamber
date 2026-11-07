//! The review flow, as `ReviewFlowDialog`, `useReviewFlowStart`,
//! `lib/reviewFlow.ts`, `AutoReviewStrip` and `useAutoReviewStore` run it.
//!
//! An automatic review belongs to the server: this window prepares the
//! review session, hands the server every prompt the run will send, and from
//! then on only projects the run the server reports and stops or dismisses
//! it. A manual review sends its prompts from here, once: the handoff request,
//! then the review prompt in the review session, which opens. Every task
//! completes against the client, runtime and session it captured; an answer
//! for a runtime that is no longer current changes nothing.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::auto_review::{AutoReviewProjection, AutoReviewRun, owner_of};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::magic_prompt::MagicPrompts;
use openchamber_native_core::permission_mode::PermissionMode;
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_core::session::{LiveStatus, SessionRecord};
use openchamber_native_core::transcript::Role;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::prompt::Prompt;
use openchamber_native_transport::prompt_parts::{PromptParts, SyntheticContext};
use openchamber_native_transport::review::{
    AutoReviewHandoff, AutoReviewMutation, AutoReviewPrompts, AutoReviewStart, AutoReviewUpdate,
    ReviewSession,
};

use crate::connection::ConnectionState;
use crate::network::NetworkTask;
use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;

/// How long a manual review waits for the handoff, and how often it looks.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(180);
const HANDOFF_POLL: Duration = Duration::from_millis(400);

/// The review dialog while it is open (`ReviewFlowExecution`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewDialog {
    pub key: SessionKey,
    pub directory: String,
    pub model: Option<ModelChoice>,
    pub agent: Option<String>,
    pub generate_handoff: bool,
    pub auto_review: bool,
    pub submitting: bool,
}

/// The review state of the window.
#[derive(Default)]
pub(crate) struct Reviews {
    pub projection: AutoReviewProjection,
    pub dialog: Option<ReviewDialog>,
    /// The run whose dispute is unfolded: session and start time.
    pub unfolded: Option<(SessionId, i64)>,
    /// Bumped on a runtime switch; answers for an older one are dropped.
    generation: u64,
    /// The connection whose host snapshot was read, and the spaces read on it.
    read_connection: Option<u64>,
    space_reads: HashSet<String>,
    tasks: HashMap<u64, Task<()>>,
    next_task: u64,
}

impl Reviews {
    fn spawn(&mut self, task: impl FnOnce(u64) -> Task<()>) {
        let id = self.next_task;
        self.next_task += 1;
        let task = task(id);
        self.tasks.insert(id, task);
    }

    /// Leaves a runtime: its runs belong to its server.
    pub(crate) fn switch_runtime(&mut self) {
        self.generation += 1;
        self.projection = AutoReviewProjection::default();
        self.dialog = None;
        self.unfolded = None;
        self.read_connection = None;
        self.space_reads.clear();
    }
}

/// The prompts of one review, rendered once from the overrides read for it.
struct ReviewPrompts {
    overrides: HashMap<String, String>,
}

impl ReviewPrompts {
    fn render(&self, id: &str, variables: &[(&str, &str)]) -> String {
        let variables = variables
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        MagicPrompts {
            overrides: &self.overrides,
            defaults: crate::starters::MagicCatalog::get().templates(),
        }
        .render(id, &variables)
        .unwrap_or_default()
    }

    /// The prompts an automatic run sends, placeholders kept for the server.
    fn auto_review(&self, with_handoff: bool) -> (Option<AutoReviewHandoff>, AutoReviewPrompts) {
        let handoff = with_handoff.then(|| AutoReviewHandoff {
            text: self.render("session.reviewHandoff.visible", &[]),
            instructions: self.render("session.reviewHandoff.instructions", &[]),
        });
        let prompts = AutoReviewPrompts {
            review_start: if with_handoff {
                self.render(
                    "session.reviewSession.visible",
                    &[("handoff", "{{handoff}}")],
                )
            } else {
                self.render("session.reviewSessionWithoutHandoff.visible", &[])
            },
            review_rules: self.render("session.review.instructions", &[]),
            feedback_to_implementer: self.render(
                "session.reviewFeedbackToImplementer.visible",
                &[("review_feedback", "{{review_feedback}}")],
            ),
            response_to_reviewer: self.render(
                "session.implementationResponseToReviewer.visible",
                &[("implementation_response", "{{implementation_response}}")],
            ),
        };
        (handoff, prompts)
    }
}

/// The reviewer as the dialog chose it.
#[derive(Clone)]
struct Reviewer {
    model: ModelChoice,
    agent: Option<String>,
    /// The reviewed session's permission mode, which a new review session
    /// starts with.
    mode: PermissionMode,
}

/// What a review start needs from the window, captured at the click.
struct ReviewStart {
    client: ServerClient,
    original: SessionId,
    directory: String,
    review_id: SessionId,
    reviewer: Reviewer,
    generate_handoff: bool,
}

impl ReviewStart {
    async fn review_session(&self) -> Result<ReviewSession, TransportError> {
        let review = self
            .client
            .create_or_reuse_review_session(
                &self.original,
                &self.directory,
                self.review_id.clone(),
                &self.reviewer.model,
                self.reviewer.agent.as_deref(),
            )
            .await?;
        if review.created {
            // A review session runs tools too, so it starts with the mode of
            // the session it reviews; a failure leaves the server's default.
            let _ = self
                .client
                .set_permission_mode(&review.record.id, self.reviewer.mode, Some(&self.directory))
                .await;
        }
        Ok(review)
    }

    /// `startAutoReview`: the review session on the dialog's model and
    /// agent, then the run.
    async fn auto_review(
        self,
        prompts: ReviewPrompts,
    ) -> Result<AutoReviewMutation, TransportError> {
        let review = self.review_session().await?;
        if !review.created {
            // A kept review session still runs on what it had then, and the
            // server's prompts switch nothing.
            if review.record.model.as_ref() != Some(&self.reviewer.model) {
                self.client
                    .select_session_model(&review.record.id, &self.reviewer.model)
                    .await?;
            }
            if let Some(agent) = &self.reviewer.agent
                && review.record.agent.as_ref() != Some(agent)
            {
                self.client
                    .select_session_agent(&review.record.id, agent)
                    .await?;
            }
        }
        let (handoff, prompts) = prompts.auto_review(self.generate_handoff);
        self.client
            .start_auto_review(&AutoReviewStart {
                original_session: self.original.clone(),
                review_session: review.record.id,
                directory: self.directory.clone(),
                handoff,
                prompts,
            })
            .await
    }
}

/// One plain prompt with its context ahead of it (`sendPlainMessage`); the
/// model and agent are the session's, or the reviewer's for its own session.
async fn send_plain(
    client: &ServerClient,
    session: &SessionId,
    ids: (MessageId, MessageId),
    text: String,
    instructions: String,
    selection: Option<&Reviewer>,
) -> Result<(), TransportError> {
    let (context, prompt) = ids;
    client
        .send_prompt(
            session,
            &Prompt {
                message_id: prompt,
                text,
                parts: PromptParts {
                    context: vec![SyntheticContext {
                        id: context,
                        text: instructions,
                        description: None,
                        metadata: None,
                    }],
                    ..PromptParts::default()
                },
                model: selection.map(|reviewer| reviewer.model.clone()),
                agent: selection.and_then(|reviewer| reviewer.agent.clone()),
                delivery: None,
            },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.source)
}

impl Workspace {
    fn review_client(&self) -> Option<(ServerClient, tokio::runtime::Handle, u64)> {
        let connection = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)?;
        Some((
            connection.client.clone()?,
            connection.network.handle(),
            connection.generation,
        ))
    }

    /// Reads the host's runs on every event-stream connection: broadcasts
    /// are not replayed, and a read is how a gap heals.
    pub(crate) fn load_auto_reviews(&mut self, cx: &mut Context<Self>) {
        let Some((_, _, generation)) = self.review_client() else {
            return;
        };
        self.reviews.read_connection = Some(generation);
        self.reviews.space_reads.clear();
        self.read_auto_review_owner(None, cx);
    }

    /// Reads the selected session's space once per connection, since that
    /// space's own server owns its runs (`refreshSpace`).
    pub(crate) fn ensure_space_auto_reviews(&mut self, cx: &mut Context<Self>) {
        let Some(space) = self
            .selected_session()
            .and_then(|session| session.resolved_directory())
            .and_then(|directory| owner_of(&directory).map(str::to_owned))
        else {
            return;
        };
        if self.reviews.read_connection.is_none() || !self.reviews.space_reads.insert(space.clone())
        {
            return;
        }
        self.read_auto_review_owner(Some(space), cx);
    }

    fn read_auto_review_owner(&mut self, space: Option<String>, cx: &mut Context<Self>) {
        let Some((client, executor, _)) = self.review_client() else {
            return;
        };
        let owner = space.clone();
        let read = NetworkTask::spawn_on(&executor, async move {
            client.auto_review_runs(owner.as_deref()).await
        });
        let generation = self.reviews.generation;
        self.reviews.spawn(|id| {
            cx.spawn(async move |this, cx| {
                let result = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.reviews.tasks.remove(&id);
                    if generation != this.reviews.generation {
                        return;
                    }
                    match result {
                        Some(Ok(snapshot)) => {
                            this.reviews.projection.apply_snapshot(
                                space.as_deref(),
                                snapshot.revision,
                                snapshot.runs,
                            );
                            cx.notify();
                        }
                        // A failed read keeps what the projection held.
                        Some(Err(error)) => {
                            tracing::warn!(%error, "the auto-review runs could not be read");
                        }
                        None => {}
                    }
                });
            })
        });
    }

    /// A run the server broadcast after a change.
    pub(crate) fn apply_auto_review_update(&mut self, update: AutoReviewUpdate) {
        self.reviews.projection.apply_update(
            update.space.as_deref(),
            update.revision,
            &update.original_session,
            &update.directory,
            update.run,
        );
    }

    /// The selected chat's run, read under the owner of its own directory.
    pub(crate) fn selected_auto_review(&self) -> Option<&AutoReviewRun> {
        let session = self.selected_session()?;
        let directory = session.resolved_directory()?;
        self.reviews.projection.run(&session.id, &directory)
    }

    /// Stop on a running run, Dismiss on a finished one. The row goes at
    /// once; a failure puts it back and rereads the owner's runs.
    pub(crate) fn remove_auto_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.selected_auto_review().cloned() else {
            return;
        };
        let Some((client, executor, _)) = self.review_client() else {
            return;
        };
        let directory = run.directory.clone();
        let Some(taken) = self
            .reviews
            .projection
            .take(&run.original_session, &directory)
        else {
            return;
        };
        let original = run.original_session.clone();
        let request_directory = directory.clone();
        let request = NetworkTask::spawn_on(&executor, async move {
            client
                .remove_auto_review(&original, &request_directory)
                .await
        });
        let generation = self.reviews.generation;
        self.reviews.spawn(|id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.reviews.tasks.remove(&id);
                    if generation != this.reviews.generation {
                        return;
                    }
                    let owner = owner_of(&directory).map(str::to_owned);
                    match result {
                        Some(Ok(mutation)) => {
                            this.reviews.projection.apply_update(
                                owner.as_deref(),
                                mutation.revision,
                                &taken.original_session,
                                &directory,
                                mutation.run,
                            );
                        }
                        _ => {
                            // The server still has it: show it again and let
                            // its state decide.
                            this.reviews.projection.restore(taken);
                            this.read_auto_review_owner(owner, cx);
                            this.notify_mutation_failure(
                                "chat.autoReview.toast.removeFailed",
                                window,
                                cx,
                            );
                        }
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    /// Open on the row: the review session, in the chat.
    pub(crate) fn open_auto_review_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self
            .selected_auto_review()
            .map(|run| run.review_session.clone())
        else {
            return;
        };
        self.select_session(review, window, cx);
    }

    /// Opens a manual review's session, which may not be listed yet.
    fn open_review_session(
        &mut self,
        review: SessionRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = review.id.clone();
        if self.sessions.get(&id).is_none() {
            self.apply_server_events(vec![ServerEvent::SessionCreated(review)], window, cx);
        }
        self.select_session(id, window, cx);
    }

    /// Unfolds or folds the selected run's dispute.
    pub(crate) fn toggle_auto_review_dispute(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.selected_auto_review() else {
            return;
        };
        let id = (run.original_session.clone(), run.started_at);
        self.reviews.unfolded = if self.reviews.unfolded.as_ref() == Some(&id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    /// `/handoff-review`: takes the composer and opens the review dialog.
    pub(crate) fn run_review_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.is_synthetic() || self.selected.is_none() {
            return false;
        }
        let text = self.composer.read(cx).value().to_string();
        let command = openchamber_native_core::magic_prompt::parse_slash_command(&text);
        if command.is_none_or(|command| command.name != "handoff-review") {
            return false;
        }
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        self.save_composer_draft(cx);
        self.queue_persistence(cx);
        self.open_review_dialog(window, cx);
        true
    }

    /// Opens the review dialog on the composer's model and agent.
    pub(crate) fn open_review_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let Some(directory) = self
            .selected_session()
            .and_then(|record| record.resolved_directory())
        else {
            self.notify_mutation_failure(
                "diffView.reviewDialog.toast.noSessionDirectory",
                window,
                cx,
            );
            return;
        };
        let selection = self.send_selection().map(|(_, selection)| selection);
        self.reviews.dialog = Some(ReviewDialog {
            key: self.mutation_key(&session),
            directory,
            model: selection
                .as_ref()
                .and_then(|selection| selection.model.clone()),
            agent: selection.and_then(|selection| selection.agent_name),
            generate_handoff: true,
            auto_review: false,
            submitting: false,
        });
        self.show_review_dialog(window, cx);
    }

    pub(crate) fn toggle_review_handoff(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self
            .reviews
            .dialog
            .as_mut()
            .filter(|dialog| !dialog.submitting)
        {
            dialog.generate_handoff = !dialog.generate_handoff;
            cx.notify();
        }
    }

    pub(crate) fn toggle_review_auto(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self
            .reviews
            .dialog
            .as_mut()
            .filter(|dialog| !dialog.submitting)
        {
            dialog.auto_review = !dialog.auto_review;
            cx.notify();
        }
    }

    pub(crate) fn close_review_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .reviews
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.submitting)
        {
            return;
        }
        self.reviews.dialog = None;
        gpui_kit::component::WindowExt::close_dialog(window, cx);
        cx.notify();
    }

    /// Review: starts the flow for the dialog's session. The dialog closes
    /// once the flow is under way: an automatic run once the server took it,
    /// a manual one once its first prompt was sent.
    pub(crate) fn start_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self
            .reviews
            .dialog
            .clone()
            .filter(|dialog| !dialog.submitting)
        else {
            return;
        };
        let Some(model) = dialog.model.clone() else {
            return;
        };
        let Some((client, executor, _)) = self.review_client() else {
            return;
        };
        let start = ReviewStart {
            client: client.clone(),
            original: dialog.key.session.clone(),
            directory: dialog.directory.clone(),
            review_id: self.session_ids.next(),
            reviewer: Reviewer {
                model,
                agent: dialog.agent.clone(),
                mode: self.session_permission_mode(&dialog.key.session),
            },
            generate_handoff: dialog.generate_handoff,
        };
        if let Some(open) = &mut self.reviews.dialog {
            open.submitting = true;
        }
        let directory = dialog.directory.clone();
        let read_prompts = NetworkTask::spawn_on(&executor, async move {
            client.magic_prompt_overrides(Some(&directory)).await
        });
        let generation = self.reviews.generation;
        let ids = (
            self.message_ids.next().0,
            self.message_ids.next().0,
            self.message_ids.next().0,
            self.message_ids.next().0,
        );
        self.reviews.spawn(|id| {
            cx.spawn_in(window, async move |this, cx| {
                let prompts = match read_prompts.await {
                    Some(Ok(overrides)) => Some(ReviewPrompts { overrides }),
                    _ => None,
                };
                let outcome = match prompts {
                    None => Err(()),
                    Some(prompts) if dialog.auto_review => {
                        let started =
                            NetworkTask::spawn_on(&executor, start.auto_review(prompts)).await;
                        match started {
                            Some(Ok(mutation)) => Ok(Some(mutation)),
                            _ => Err(()),
                        }
                    }
                    Some(prompts) => {
                        let request = NetworkTask::spawn_on(
                            &executor,
                            manual_review_first_step(start, prompts, ids),
                        );
                        match request.await {
                            Some(Ok(next)) => {
                                let _ = this.update_in(cx, |this, window, cx| {
                                    this.continue_manual_review(next, generation, window, cx);
                                });
                                Ok(None)
                            }
                            _ => Err(()),
                        }
                    }
                };
                let _ = this.update_in(cx, |this, window, cx| {
                    this.reviews.tasks.remove(&id);
                    if generation != this.reviews.generation {
                        return;
                    }
                    if let Some(open) = &mut this.reviews.dialog {
                        open.submitting = false;
                    }
                    match outcome {
                        Ok(mutation) => {
                            if let Some(mutation) = mutation {
                                this.reviews.projection.apply_update(
                                    owner_of(&dialog.directory),
                                    mutation.revision,
                                    &dialog.key.session,
                                    &dialog.directory,
                                    mutation.run,
                                );
                            }
                            if this.reviews.dialog.as_ref().map(|open| &open.key)
                                == Some(&dialog.key)
                            {
                                this.close_review_dialog(window, cx);
                            }
                        }
                        Err(()) => this.notify_mutation_failure(
                            "diffView.reviewDialog.toast.startFailed",
                            window,
                            cx,
                        ),
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    /// The manual review after its first step: without a handoff the review
    /// session is ready and opens; with one, the handoff is awaited in the
    /// background, then the review prompt goes to the review session.
    fn continue_manual_review(
        &mut self,
        next: ManualReview,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match next {
            ManualReview::Opened(review) => {
                if generation == self.reviews.generation {
                    self.open_review_session(*review, window, cx);
                }
            }
            ManualReview::AwaitingHandoff(pending) => {
                self.reviews.spawn(|id| {
                    cx.spawn_in(window, async move |this, cx| {
                        let handoff = wait_for_handoff(&this, cx, &pending).await;
                        let request = handoff.map(|handoff| {
                            NetworkTask::spawn_on(
                                &pending.executor.clone(),
                                pending.review(handoff),
                            )
                        });
                        let result = match request {
                            Some(request) => request.await.and_then(Result::ok),
                            None => None,
                        };
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.reviews.tasks.remove(&id);
                            if generation != this.reviews.generation {
                                return;
                            }
                            match result {
                                Some(review) => this.open_review_session(review, window, cx),
                                None => tracing::warn!("the background review flow did not finish"),
                            }
                        });
                    })
                });
            }
        }
    }
}

/// Where a manual review stands after its first step.
enum ManualReview {
    /// The review session got its prompt.
    Opened(Box<SessionRecord>),
    /// The handoff request was sent; its answer is awaited.
    AwaitingHandoff(Box<PendingHandoff>),
}

/// A manual review waiting for the reviewed session's handoff.
struct PendingHandoff {
    start: ReviewStart,
    prompts: ReviewPrompts,
    executor: tokio::runtime::Handle,
    /// When the handoff request left; the answer is the newest reply after it.
    sent_at: i64,
    review_ids: (MessageId, MessageId),
}

impl PendingHandoff {
    /// The review session with its prompt, the handoff filled in.
    async fn review(self, handoff: String) -> Result<SessionRecord, TransportError> {
        let review = self.start.review_session().await?;
        let text = self
            .prompts
            .render("session.reviewSession.visible", &[("handoff", &handoff)]);
        send_plain(
            &self.start.client,
            &review.record.id,
            self.review_ids,
            text,
            self.prompts.render("session.review.instructions", &[]),
            Some(&self.start.reviewer),
        )
        .await?;
        Ok(review.record)
    }
}

async fn manual_review_first_step(
    start: ReviewStart,
    prompts: ReviewPrompts,
    ids: (MessageId, MessageId, MessageId, MessageId),
) -> Result<ManualReview, TransportError> {
    let (handoff_context, handoff_prompt, review_context, review_prompt) = ids;
    if !start.generate_handoff {
        let review = start.review_session().await?;
        send_plain(
            &start.client,
            &review.record.id,
            (review_context, review_prompt),
            prompts.render("session.reviewSessionWithoutHandoff.visible", &[]),
            prompts.render("session.review.instructions", &[]),
            Some(&start.reviewer),
        )
        .await?;
        return Ok(ManualReview::Opened(Box::new(review.record)));
    }
    let sent_at = now_ms();
    send_plain(
        &start.client,
        &start.original,
        (handoff_context, handoff_prompt),
        prompts.render("session.reviewHandoff.visible", &[]),
        prompts.render("session.reviewHandoff.instructions", &[]),
        None,
    )
    .await?;
    Ok(ManualReview::AwaitingHandoff(Box::new(PendingHandoff {
        executor: tokio::runtime::Handle::current(),
        start,
        prompts,
        sent_at,
        review_ids: (review_context, review_prompt),
    })))
}

/// `waitForAssistantText`: once the reviewed session and its subagents are
/// idle, the newest completed reply after the request that has text. `None`
/// after the timeout, a runtime switch or a closed window.
async fn wait_for_handoff(
    this: &gpui_kit::WeakEntity<Workspace>,
    cx: &mut gpui_kit::AsyncWindowContext,
    pending: &PendingHandoff,
) -> Option<String> {
    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    let session = pending.start.original.clone();
    while Instant::now() < deadline {
        cx.background_executor().timer(HANDOFF_POLL).await;
        let text = this
            .update(cx, |this, cx| {
                this.ensure_open(&session, cx);
                this.handoff_text(&session, pending.sent_at)
            })
            .ok()?;
        if text.is_some() {
            return text;
        }
    }
    None
}

impl Workspace {
    /// The handoff once the session's turn is over.
    fn handoff_text(&self, session: &SessionId, after: i64) -> Option<String> {
        let busy = |id: &SessionId| {
            matches!(
                self.live_status.get(id),
                Some(LiveStatus::Busy | LiveStatus::Retry)
            )
        };
        if busy(session)
            || self
                .sessions
                .active()
                .iter()
                .any(|child| child.parent_id.as_ref() == Some(session) && busy(&child.id))
        {
            return None;
        }
        let open = self.open_sessions.get(session)?;
        let mut replies: Vec<_> = open
            .transcript
            .messages()
            .iter()
            .filter(|message| {
                message.info.role == Role::Assistant
                    && message.info.created >= after - 1000
                    && message.info.completed.is_some()
            })
            .collect();
        replies.sort_by_key(|message| std::cmp::Reverse(message.info.created));
        replies
            .into_iter()
            .map(|message| message.plain_text().trim().to_owned())
            .find(|text| !text.is_empty())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
