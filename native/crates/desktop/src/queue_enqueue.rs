//! Explicit payload enqueue recovery and shared named-byte retirement.
//! Hydration starts no work. Queue and attachment retirement intents commit
//! together before either release, preserving each failed blob for explicit retry.

use std::collections::{HashMap, HashSet};

use gpui_kit::{AsyncWindowContext, Context, Task, WeakEntity, Window};
use openchamber_native_core::draft::{DraftKey, DraftTarget};
use openchamber_native_core::draft_content::{DraftFileContent, Mention};
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::queue_manifest::{
    QueueAgent, QueueAttachmentDescriptor, QueueEnqueueRecord, QueueEnqueueState,
    QueueLocalCapture, QueueLookup, QueueManifestAttachment, QueueManifestItem, QueueMention,
    QueuePayloadDocument, QueuePayloadEnqueue, QueuePayloadItem, QueuePayloadUse,
    QueuePromptContext, QueuePromptParts, QueueRecord, QueueRequestOwner, QueueRetry, QueueScope,
    QueueSkill, QueueUpload,
};

use crate::attachment_io::AttachmentFailure;
use crate::attachment_retirement::{self, RetirementStart};
use crate::attachment_spool::{BlobDescriptor, BlobId, SpoolCommit};
use crate::composer_document::{self, ComposerCapture};
use crate::composer_payload::{self, PreparedComposerInput};
use crate::connection::ConnectionState;
use crate::journal_worker::BlobFailure;
use crate::mutation_journal::JournalError;
use crate::network::NetworkTask;
use crate::queue_capture::{
    QueueJson, QueueNativeSource, QueueObjectKind, QueueOperationId, sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    EnqueueSourceCapture, QueueAction, QueueConfiguration, QueueEnqueuePhase,
    QueueNativeFileDescriptor, QueueOperation, QueueOwner, QueueSource, QueueTakePurpose,
};
use crate::queue_take::QueueServer;
use crate::workspace::Workspace;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueCheck {
    Unknown,
    Checking,
    Preparing,
    Failed,
    NotObserved,
    Queued,
}

struct EnqueueTask {
    runtime: RuntimeId,
    owner: Option<DraftKey>,
    native_roots: Vec<AttachmentId>,
    _task: Task<()>,
}

#[cfg(test)]
struct EnqueueCaptureGate {
    entered: tokio::sync::oneshot::Sender<ComposerCapture>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Default)]
pub(crate) struct EnqueueState {
    tasks: HashMap<QueueOperationId, EnqueueTask>,
    checks: HashMap<QueueOperationId, EnqueueCheck>,
    /// Display-only receipt state, decoded at hydration/receipt transitions.
    deliveries: HashMap<QueueOperationId, QueueEnqueueState>,
    /// Accepted enqueues this window saw queued: by their own receipt when it
    /// was accepted here, or listed by their queue. Leaving the queue from
    /// here is the one transition that starts a status check on its own.
    listed: HashSet<QueueOperationId>,
    /// Enqueues whose queue is about to be answered again: after their
    /// acceptance, or after a local removal. Until then a queue that does
    /// not list them proves nothing.
    awaiting: HashSet<QueueOperationId>,
    /// Enqueues whose departure already started its one automatic check.
    automatic: HashSet<QueueOperationId>,
    /// Enqueues whose capturing activation is still running.
    initial: HashSet<QueueOperationId>,
    /// Exact expanded bytes survive a failed put for same-window explicit retry.
    /// A restart with incomplete inputs cannot rerun preparation.
    pending_inputs: HashMap<QueueOperationId, Vec<u8>>,
    #[cfg(test)]
    capture_gates: HashMap<QueueOperationId, EnqueueCaptureGate>,
}

impl EnqueueState {
    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.tasks.values().any(|task| &task.runtime == runtime)
    }

    pub(crate) fn check(&self, id: &QueueOperationId) -> EnqueueCheck {
        self.checks
            .get(id)
            .copied()
            .unwrap_or(EnqueueCheck::Unknown)
    }

    pub(crate) fn any_busy(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub(crate) fn has_native_root_holds(&self) -> bool {
        self.tasks
            .values()
            .any(|task| !task.native_roots.is_empty())
    }

    pub(crate) fn busy(&self, id: &QueueOperationId) -> bool {
        self.tasks.contains_key(id)
    }

    /// Whether the activation that captured `id` is still running.
    pub(crate) fn first_activation_running(&self, id: &QueueOperationId) -> bool {
        self.initial.contains(id) && self.tasks.contains_key(id)
    }

    pub(crate) fn seen_listed(&self, id: &QueueOperationId) -> bool {
        self.listed.contains(id)
    }

    pub(crate) fn awaiting_listing(&self, id: &QueueOperationId) -> bool {
        self.awaiting.contains(id)
    }

    /// Whether the check started by `id` leaving its queue is still running.
    pub(crate) fn automatic_check_running(&self, id: &QueueOperationId) -> bool {
        self.automatic.contains(id) && self.tasks.contains_key(id)
    }

    /// The queue lists `id`; a later departure may be checked once more.
    pub(crate) fn note_listed(&mut self, id: QueueOperationId) {
        self.automatic.remove(&id);
        self.listed.insert(id);
    }

    /// `id` left its queue: the departure is consumed, whether or not its
    /// check can start.
    pub(crate) fn note_left(&mut self, id: &QueueOperationId) {
        self.listed.remove(id);
        self.automatic.insert(id.clone());
    }

    pub(crate) fn await_listing(&mut self, id: QueueOperationId) {
        self.awaiting.insert(id);
    }

    pub(crate) fn settle_awaited(&mut self, id: &QueueOperationId) {
        self.awaiting.remove(id);
    }

    /// Drops what was learned from a projection that is being reset.
    pub(crate) fn reset_listings(&mut self) {
        self.listed.clear();
        self.awaiting.clear();
        self.automatic.clear();
    }

    fn forget_listing(&mut self, id: &QueueOperationId) {
        self.listed.remove(id);
        self.awaiting.remove(id);
        self.automatic.remove(id);
    }

    /// Before admission the actor still owns the captured native files and their
    /// source-document groups. The ledger takes over those roots at admission.
    pub(crate) fn references_native(&self, id: &AttachmentId) -> bool {
        self.tasks
            .values()
            .any(|task| task.native_roots.contains(id))
    }

    pub(crate) fn blocks_session(&self, runtime: &RuntimeId, session: &SessionId) -> bool {
        self.tasks.values().any(|task| {
            &task.runtime == runtime
                && task
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.target() == &DraftTarget::Session(session.clone()))
        })
    }

    pub(crate) fn delivery(&self, id: &QueueOperationId) -> Option<QueueEnqueueState> {
        self.deliveries.get(id).copied()
    }

    pub(crate) fn restore(&mut self, queue: &crate::queue_storage::QueueStorage) {
        #[derive(serde::Deserialize)]
        struct Delivery {
            state: QueueEnqueueState,
        }

        self.deliveries.clear();
        for operation in queue.operations() {
            if let QueueAction::Enqueue {
                phase:
                    QueueEnqueuePhase::Accepted { receipt, .. }
                    | QueueEnqueuePhase::RetryPrepared { receipt, .. },
                ..
            } = operation.action()
                && let Ok(delivery) = serde_json::from_slice::<Delivery>(receipt.bytes())
            {
                self.deliveries
                    .insert(operation.id().clone(), delivery.state);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activation {
    Initial,
    Check,
    Retry,
}

/// Kept in the window-owned task while disk barriers hold no Tokio tasks.
struct CapturedServer {
    runtime: RuntimeId,
    client: ServerClient,
    executor: tokio::runtime::Handle,
    owner: QueueRequestOwner,
}

struct InitialEnqueue {
    capture: ComposerCapture,
    configuration: QueuedSendConfig,
    native_context_ids: Vec<MessageId>,
    skill_context_ids: Vec<MessageId>,
    files: Vec<QueueNativeFileDescriptor>,
}

/// Both namespaces have durable intents before either kind is released.
pub(crate) struct QueueRetirement {
    pub(crate) chunks: Vec<BlobId>,
    pub(crate) attachments: Vec<BlobId>,
}

#[derive(Clone, Copy)]
enum RetiredByte {
    Queue(BlobId),
    Attachment(BlobId),
}

impl Workspace {
    /// Unresolved enqueue ownership cannot become a second enqueue identity.
    pub(crate) fn enqueue_blocks_owner(&self, owner: &DraftKey) -> bool {
        self.queue
            .enqueues
            .tasks
            .values()
            .any(|task| task.owner.as_ref() == Some(owner))
            || self.queue_storage.operations().iter().any(|operation| {
                operation
                    .owner()
                    .draft_key()
                    .as_ref()
                    .is_ok_and(|held| held == owner)
                    && matches!(
                        operation.action(),
                        QueueAction::Enqueue {
                            phase: QueueEnqueuePhase::Capturing
                                | QueueEnqueuePhase::Ready { .. }
                                | QueueEnqueuePhase::Unknown { .. },
                            ..
                        }
                    )
            })
    }

    pub(crate) fn begin_payload_enqueue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.persistence_ready() || self.close_in_progress() {
            return;
        }
        let Some(session) = self.selected_session() else {
            return;
        };
        let session_id = session.id.clone();
        let Some(directory) = session.directory.clone() else {
            return;
        };
        let Some(owner) = self.composer_key.clone().filter(|owner| {
            owner.runtime() == &self.runtime
                && owner.target() == &DraftTarget::Session(session_id.clone())
        }) else {
            return;
        };
        if self.enqueue_blocks_owner(&owner)
            || self.queue_preview_pending(&session_id)
            || !self.composer_document_ready(&owner)
            || self.attachment_storage.has_pending(&owner)
            || self
                .attachment_preparations
                .iter()
                .any(|hold| hold.owner == owner)
        {
            return;
        }
        // The document owner captures source pieces, exclusions and its exact
        // revision before configuration choices or the first asynchronous step.
        let capture = match self.capture_composer_submission(cx) {
            Ok(capture) => capture,
            Err(_) => {
                self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                return;
            }
        };
        if capture.composer.native.owner != owner {
            return;
        }
        if !self
            .compound_composer_sendable()
            .unwrap_or_else(|| capture.composer.native.draft.has_sendable_content())
        {
            return;
        }
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        let Some(model) = selection.model else {
            return;
        };
        let configuration = QueuedSendConfig {
            provider_id: model.provider_id,
            model_id: model.model_id,
            agent: selection.agent_name,
            variant: model.variant,
        };
        let source = &capture.composer.native;
        // Reference-only files have no captured byte authority. Never open their
        // URI or substitute an empty file; the complete composer stays untouched.
        if source
            .draft
            .content
            .files
            .iter()
            .any(|file| !matches!(file.content, DraftFileContent::Prepared))
        {
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
            return;
        }
        // Protocol-only refusal precedes ownership. Placeholder server roots
        // validate metadata and encoding only; they grant no upload or byte proof.
        let mut native_files = Vec::new();
        let planned = source
            .draft
            .content
            .files
            .iter()
            .enumerate()
            .map(|(ordinal, file)| {
                let prepared = self
                    .attachments
                    .get(&file.id)
                    .ok_or(QueueCaptureFailure::Invalid)?;
                if file.byte_len != Some(prepared.blob.byte_len()) {
                    return Err(QueueCaptureFailure::Invalid);
                }
                native_files.push(QueueNativeFileDescriptor {
                    id: file.id.to_string(),
                    byte_len: prepared.blob.byte_len(),
                    sha256: *prepared.blob.sha256(),
                });
                Ok(QueueAttachmentDescriptor {
                    blob_id: sha256(format!("preflight-file-{ordinal}").as_bytes()),
                    request_id: "native_preflight".into(),
                    attachment_id: format!("file-{ordinal}"),
                    filename: upload_filename(
                        file.name.as_deref().ok_or(QueueCaptureFailure::Invalid)?,
                        ordinal,
                    ),
                    mime_type: file
                        .media_type
                        .clone()
                        .ok_or(QueueCaptureFailure::Invalid)?,
                    size: prepared.blob.byte_len(),
                    sha256: *prepared.blob.sha256(),
                })
            })
            .collect::<Result<Vec<_>, QueueCaptureFailure>>();
        let valid = planned.and_then(|files| {
            let item = manifest(source, configuration.clone(), files)?;
            QueuePayloadDocument::from_manifest(&directory, item, QueuePayloadUse::Composer, sha256)
                .map(|_| ())
                .map_err(Into::into)
        });
        if valid.is_err() {
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Ok(scope) = QueueScope::new(&client, session_id, &directory) else {
            return;
        };
        let Ok(id) = QueueOperationId::new(self.message_ids.next().0.as_str()) else {
            return;
        };
        let Ok(request_owner) = scope.request(id.as_str()) else {
            return;
        };
        let server = CapturedServer {
            runtime: self.runtime.clone(),
            client,
            executor: connection.network.handle(),
            owner: request_owner,
        };
        let native_context_ids: Vec<_> = (0..source
            .draft
            .content
            .context
            .iter()
            .map(|context| {
                usize::from(context.instructions.is_some())
                    + usize::from(!context.text.trim().is_empty())
            })
            .sum::<usize>())
            .map(|_| self.message_ids.next().0)
            .collect();
        let skill_context_ids: Vec<_> = (0..crate::mutation_journal::MAX_ENTRIES)
            .map(|_| self.message_ids.next().0)
            .collect();
        let Ok(queue_owner) = QueueOwner::new(
            &server.runtime,
            &directory,
            server.owner.scope().session(),
            &owner,
        ) else {
            return;
        };
        let operation_id = id.clone();
        let config = QueueConfiguration::capture(&configuration);
        let contexts = native_context_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let files = native_files.clone();
        if self
            .register_composer_submission(
                id.clone(),
                capture.clone(),
                skill_context_ids.clone(),
                move |capture| {
                    let mut operation = QueueOperation::enqueue_with_edit(
                        operation_id.clone(),
                        queue_owner.clone(),
                        config.clone(),
                        capture.composer.clone(),
                        contexts.clone(),
                        files.clone(),
                        capture.edit.clone(),
                    )?;
                    operation.begin_composer_preparation(capture.generation, capture.revision)?;
                    Ok(operation)
                },
            )
            .is_err()
        {
            self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
            return;
        }
        self.spawn_enqueue_activation(
            id,
            server,
            Activation::Initial,
            Some(InitialEnqueue {
                capture,
                configuration,
                native_context_ids,
                skill_context_ids,
                files: native_files,
            }),
            Some(owner),
            window,
            cx,
        );
    }

    pub(crate) fn check_payload_enqueue(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_enqueue_activation(id, Activation::Check, window, cx);
    }

    pub(crate) fn retry_payload_enqueue(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_enqueue_activation(id, Activation::Retry, window, cx);
    }

    /// Retries named local retirement intents only. No server mutation runs here.
    pub(crate) fn retry_enqueue_retirement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_in_progress() || !self.persistence_loaded() || self.queue.any_payload_busy() {
            return;
        }
        let Ok(blobs) = self.queue_storage.retired_blobs() else {
            return;
        };
        let plan = QueueRetirement {
            chunks: blobs,
            attachments: self.retired_attachment_blobs.clone(),
        };
        if plan.chunks.is_empty() && plan.attachments.is_empty() {
            return;
        }
        let id = QueueOperationId::new("queue_retire_local").expect("fixed local task identity");
        let task_id = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = release_retirement(&this, cx, plan).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.queue.enqueues.tasks.remove(&id);
                if result.is_err() && !this.close_in_progress() {
                    this.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
                }
                cx.notify();
            });
        });
        self.queue.enqueues.tasks.insert(
            task_id,
            EnqueueTask {
                runtime: self.runtime.clone(),
                owner: None,
                native_roots: Vec::new(),
                _task: task,
            },
        );
        cx.notify();
    }

    fn start_enqueue_activation(
        &mut self,
        id: &QueueOperationId,
        activation: Activation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() || self.queue.enqueues.busy(id) {
            return;
        }
        let Some(operation) = self.queue_storage.operation(id) else {
            return;
        };
        if operation.owner().runtime() != self.runtime.as_str()
            || !matches!(operation.action(), QueueAction::Enqueue { .. })
        {
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| connection.state == ConnectionState::Connected)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Ok(scope) = QueueScope::new(
            &client,
            SessionId::from(operation.owner().session()),
            operation.owner().directory(),
        ) else {
            return;
        };
        let Ok(owner) = scope.request(id.as_str()) else {
            return;
        };
        let held_owner = operation.owner().draft_key().ok();
        let server = CapturedServer {
            runtime: self.runtime.clone(),
            client,
            executor: connection.network.handle(),
            owner,
        };
        self.spawn_enqueue_activation(id.clone(), server, activation, None, held_owner, window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_enqueue_activation(
        &mut self,
        id: QueueOperationId,
        server: CapturedServer,
        activation: Activation,
        initial: Option<InitialEnqueue>,
        owner: Option<DraftKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let runtime = server.runtime.clone();
        let native_roots = initial
            .as_ref()
            .map(|initial| {
                initial
                    .capture
                    .composer
                    .native
                    .draft
                    .content
                    .files
                    .iter()
                    .flat_map(|file| {
                        std::iter::once(file.id.clone()).chain(file.source_document_id.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.queue.enqueues.checks.insert(
            id.clone(),
            if activation == Activation::Check {
                EnqueueCheck::Checking
            } else {
                EnqueueCheck::Preparing
            },
        );
        let task_id = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                if let Some(initial) = initial {
                    prepare_initial_compound(&this, cx, &id, &server, initial).await?;
                }
                enqueue(&this, cx, &id, &server, activation).await
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if result.is_err() {
                    this.settle_failed_composer_submission(&id);
                }
                let roots = this
                    .queue
                    .enqueues
                    .tasks
                    .get_mut(&id)
                    .map(|task| std::mem::take(&mut task.native_roots))
                    .unwrap_or_default();
                this.queue.enqueues.tasks.remove(&id);
                this.queue.enqueues.initial.remove(&id);
                this.retire_released_native_roots(roots, window, cx);
                if result.is_err() {
                    // No reread follows a failed activation.
                    this.queue.enqueues.settle_awaited(&id);
                }
                if this.queue_storage.operation(&id).is_some() {
                    this.queue.enqueues.checks.insert(
                        id,
                        match &result {
                            Ok(check) => *check,
                            Err(_) if activation == Activation::Check => EnqueueCheck::Failed,
                            Err(_) => EnqueueCheck::Unknown,
                        },
                    );
                } else {
                    this.queue.enqueues.checks.remove(&id);
                    this.queue.enqueues.deliveries.remove(&id);
                    this.queue.enqueues.forget_listing(&id);
                    this.queue.enqueues.pending_inputs.remove(&id);
                }
                if result.is_err() && this.runtime == runtime && !this.close_in_progress() {
                    this.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
                }
                this.resume_native_root_close(window, cx);
                cx.notify();
            });
        });
        if activation == Activation::Initial {
            self.queue.enqueues.initial.insert(task_id.clone());
        }
        self.queue.enqueues.tasks.insert(
            task_id,
            EnqueueTask {
                runtime: self.runtime.clone(),
                owner,
                native_roots,
                _task: task,
            },
        );
        cx.notify();
    }
}

fn compound_allowed(workspace: &Workspace, id: &QueueOperationId, server: &CapturedServer) -> bool {
    !workspace.close_in_progress()
        && workspace.persistence_ready()
        && workspace.composer_submission_dispatch_allowed(id)
        && workspace.queue.enqueues.tasks.get(id).is_some_and(|task| {
            task.runtime == server.runtime
                && task.owner.as_ref().is_some_and(|owner| {
                    owner.runtime() == &server.runtime
                        && owner.target()
                            == &DraftTarget::Session(server.owner.scope().session().clone())
                        && owner.directory()
                            == crate::attachment_catalog::remote_path(
                                server.owner.scope().directory(),
                            )
                            .as_deref()
                            .unwrap_or("")
                })
        })
        && workspace
            .queue_storage
            .operation(id)
            .is_none_or(|operation| {
                operation.owner().runtime() == server.runtime.as_str()
                    && operation.owner().directory() == server.owner.scope().directory()
                    && operation.owner().session() == server.owner.scope().session().as_str()
            })
}

#[allow(clippy::too_many_arguments)]
async fn persist_compound(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    role: &str,
    kind: QueueObjectKind,
    bytes: Vec<u8>,
    source: bool,
    activation: Activation,
) -> Result<crate::queue_capture::QueueObjectId, QueueCaptureFailure> {
    if activation == Activation::Retry {
        queue_payload_io::persist_retry(this, cx, id, role, kind, bytes, source).await
    } else {
        queue_payload_io::persist(this, cx, id, role, kind, bytes, source).await
    }
}

async fn compound_source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<EnqueueSourceCapture, QueueCaptureFailure> {
    let operation = this
        .update_in(cx, |this, _, _| this.queue_storage.operation(id).cloned())
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let QueueAction::Enqueue { source, .. } = operation.action() else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let source = match source {
        QueueSource::Inline(_) => operation.enqueue_capture()?,
        QueueSource::Companion(object) => {
            let bytes = queue_payload_io::read(this, cx, object).await?;
            cx.background_executor()
                .spawn(async move {
                    serde_json::from_slice::<EnqueueSourceCapture>(&bytes)
                        .map_err(|_| QueueCaptureFailure::Invalid)
                })
                .await?
        }
    };
    cx.background_executor()
        .spawn(async move {
            validate_compound_source(&operation, &source)?;
            Ok(source)
        })
        .await
}

fn validate_compound_source(
    operation: &QueueOperation,
    source: &EnqueueSourceCapture,
) -> Result<(), QueueCaptureFailure> {
    let bytes = serde_json::to_vec(source).map_err(|_| QueueCaptureFailure::Invalid)?;
    if !matches!(operation.action(), QueueAction::Enqueue { .. })
        || source.version != 1
        || source.composer.native.owner != operation.owner().draft_key()?
        || source.composer.references != source.intent.companions
        || source.composer.references != operation.dependencies()
        || source.intent.edit.as_ref() != operation.composer_edit()
        || source.intent.directory != operation.owner().directory()
        || operation.source_sha256() != Some(&sha256(&bytes))
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

async fn prepare_initial_compound(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &CapturedServer,
    initial: InitialEnqueue,
) -> Result<(), QueueCaptureFailure> {
    let InitialEnqueue {
        capture,
        configuration,
        native_context_ids,
        skill_context_ids,
        files,
    } = initial;
    // Only the test capture gate reads the original capture here.
    #[cfg(not(test))]
    drop(capture);
    #[cfg(test)]
    if let Some(gate) = this
        .update_in(cx, |workspace, _, _| {
            workspace.queue.enqueues.capture_gates.remove(id)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?
    {
        if gate.entered.send(capture.clone()).is_err() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        gate.release
            .await
            .map_err(|_| QueueCaptureFailure::Unavailable)?;
    }
    let capture = composer_document::complete_submission(this, cx, id, false, None).await?;
    this.update_in(cx, |workspace, _, _| {
        if let Some(task) = workspace.queue.enqueues.tasks.get_mut(id) {
            task.native_roots.clear();
        }
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if !this
        .update_in(cx, |workspace, _, _| {
            compound_allowed(workspace, id, server)
        })
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let source = compound_source(this, cx, id).await?;
    let source_sha256 = this
        .update_in(cx, |workspace, _, _| {
            workspace
                .queue_storage
                .operation(id)
                .and_then(|operation| operation.source_sha256())
                .copied()
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Invalid)?;
    let inputs = composer_document::materialize(this, cx, &capture).await?;
    if !this
        .update_in(cx, |workspace, _, _| {
            compound_allowed(workspace, id, server)
        })
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let preparation_server = QueueServer::new(
        server.runtime.clone(),
        server.client.clone(),
        server.executor.clone(),
        server.owner.scope().session().clone(),
        server.owner.scope().directory(),
    )?;
    let input = crate::composer_delivery::ComposerDeliveryInput {
        inputs,
        configuration,
        native_context_ids,
        native_files: files,
        skill_context_ids,
    };
    let delivery = crate::composer_delivery::prepare_operation(
        this,
        cx,
        id,
        &preparation_server,
        capture.generation,
        capture.revision,
        false,
        input,
    )
    .await
    .map_err(|error| match error {
        crate::composer_delivery::ComposerDeliveryError::Capture(error) => error,
        crate::composer_delivery::ComposerDeliveryError::Preparation
        | crate::composer_delivery::ComposerDeliveryError::File => QueueCaptureFailure::Invalid,
    })?;
    let generation = capture.generation;
    let revision = capture.revision;
    let (input, bytes) = cx
        .background_executor()
        .spawn(async move {
            let (local, files) = delivery.into_capture()?;
            let input =
                PreparedComposerInput::new(local, files, generation, revision, source_sha256)?;
            let bytes = input.encode()?;
            Ok::<_, QueueCaptureFailure>((input, bytes))
        })
        .await?;
    this.update_in(cx, |workspace, _, _| {
        workspace
            .queue
            .enqueues
            .pending_inputs
            .insert(id.clone(), bytes);
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    composer_payload::persist_and_seal(this, cx, id, &input, false).await?;
    this.update_in(cx, |workspace, _, _| {
        workspace.queue.enqueues.pending_inputs.remove(id);
        workspace.release_composer_submission(id);
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    finish_compound_preparation(
        this,
        cx,
        id,
        server,
        &source,
        input,
        Some(capture),
        Activation::Initial,
    )
    .await
}

async fn retry_compound_preparation(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &CapturedServer,
) -> Result<(), QueueCaptureFailure> {
    let capture = composer_document::ensure_submission_edit(this, cx, id, true).await?;
    let source = compound_source(this, cx, id).await?;
    let (source, bytes) = cx
        .background_executor()
        .spawn(async move {
            let bytes = serde_json::to_vec(&source).map_err(|_| QueueCaptureFailure::Invalid)?;
            Ok::<_, QueueCaptureFailure>((source, bytes))
        })
        .await?;
    persist_compound(
        this,
        cx,
        id,
        "source",
        QueueObjectKind::NativeCompanion,
        bytes,
        true,
        Activation::Retry,
    )
    .await?;
    let sealed = this
        .update_in(cx, |workspace, _, _| {
            workspace
                .queue_storage
                .operation(id)
                .and_then(|operation| operation.composer_preparation())
                .and_then(|plan| plan.inputs_object())
                .is_some()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    let input = if sealed {
        let input = composer_payload::restore(this, cx, id).await?;
        this.update_in(cx, |workspace, _, _| {
            workspace.release_composer_submission(id)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
        input
    } else {
        // Only bytes retained by the original actor can repair an incomplete put.
        // A restarted partial preparation remains owned and grants no fresh read.
        let bytes = this
            .update_in(cx, |workspace, _, _| {
                workspace.queue.enqueues.pending_inputs.get(id).cloned()
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)?;
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => {
                let capture = capture.as_ref().ok_or(QueueCaptureFailure::Unavailable)?;
                let skill_context_ids = this
                    .update_in(cx, |workspace, _, _| {
                        workspace.composer_submission_skill_ids(id)
                    })
                    .ok()
                    .flatten()
                    .ok_or(QueueCaptureFailure::Unavailable)?;
                let inputs = composer_document::materialize(this, cx, capture).await?;
                let configuration = this
                    .update_in(cx, |workspace, _, _| {
                        workspace
                            .queue_storage
                            .operation(id)
                            .map(|operation| operation.configuration().restore())
                    })
                    .ok()
                    .flatten()
                    .ok_or(QueueCaptureFailure::Unavailable)?;
                let preparation_server = QueueServer::new(
                    server.runtime.clone(),
                    server.client.clone(),
                    server.executor.clone(),
                    server.owner.scope().session().clone(),
                    server.owner.scope().directory(),
                )?;
                let input = crate::composer_delivery::ComposerDeliveryInput {
                    inputs,
                    configuration,
                    native_context_ids: source
                        .intent
                        .native_context_ids
                        .iter()
                        .cloned()
                        .map(MessageId::from)
                        .collect(),
                    native_files: source.intent.files.clone(),
                    skill_context_ids,
                };
                let delivery = crate::composer_delivery::prepare_operation(
                    this,
                    cx,
                    id,
                    &preparation_server,
                    capture.generation,
                    capture.revision,
                    true,
                    input,
                )
                .await
                .map_err(|_| QueueCaptureFailure::Invalid)?;
                let (local, files) = delivery.into_capture()?;
                let digest = this
                    .update_in(cx, |workspace, _, _| {
                        workspace
                            .queue_storage
                            .operation(id)
                            .and_then(|operation| operation.source_sha256())
                            .copied()
                    })
                    .ok()
                    .flatten()
                    .ok_or(QueueCaptureFailure::Invalid)?;
                let input = PreparedComposerInput::new(
                    local,
                    files,
                    capture.generation,
                    capture.revision,
                    digest,
                )?;
                let bytes = cx
                    .background_executor()
                    .spawn(async move { input.encode() })
                    .await?;
                this.update_in(cx, |workspace, _, _| {
                    workspace
                        .queue
                        .enqueues
                        .pending_inputs
                        .insert(id.clone(), bytes.clone());
                })
                .map_err(|_| QueueCaptureFailure::Unavailable)?;
                bytes
            }
        };
        let directory = server.owner.scope().directory().to_owned();
        let input = cx
            .background_executor()
            .spawn(async move { PreparedComposerInput::decode(&bytes, &directory) })
            .await?;
        composer_payload::persist_and_seal(this, cx, id, &input, true).await?;
        this.update_in(cx, |workspace, _, _| {
            workspace.queue.enqueues.pending_inputs.remove(id);
            workspace.release_composer_submission(id);
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
        input
    };
    finish_compound_preparation(
        this,
        cx,
        id,
        server,
        &source,
        input,
        None,
        Activation::Retry,
    )
    .await
}

/// The shared delivery already projected every anchor into the complete text.
/// Enqueue changes only local byte ownership into server upload descriptors.
fn compound_manifest(
    capture: &QueueLocalCapture,
    files: Vec<QueueAttachmentDescriptor>,
) -> Result<QueueManifestItem, QueueCaptureFailure> {
    let item = capture.item();
    if files.len() != item.files.len()
        || files
            .iter()
            .zip(&item.files)
            .enumerate()
            .any(|(ordinal, (upload, file))| {
                upload.size != file.byte_len
                    || upload.sha256 != file.sha256
                    || upload.filename != upload_filename(&file.name, ordinal)
                    || upload.mime_type != file.mime_type
            })
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(QueueManifestItem {
        content: item.content.clone(),
        text: item.text.clone(),
        attachment_manifest: files
            .into_iter()
            .zip(&item.files)
            .map(|(attachment, file)| QueueManifestAttachment {
                agent_path: (attachment.filename != file.name).then(|| file.name.clone()),
                attachment,
                description: file.description.clone(),
                mention: file.mention.clone(),
            })
            .collect(),
        context: Vec::new(),
        context_preview: None,
        send_config: item.send_config.clone(),
        prompt_parts: item.prompt_parts.clone(),
    })
}

/// Mirrors attachmentBasename's lexical spelling rules. Invalid upload basenames
/// use a stable internal label; the manifest separately retains the exact name.
fn upload_filename(name: &str, ordinal: usize) -> String {
    let windows_separators = name.starts_with("\\\\")
        || matches!(name.as_bytes(), [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic());
    let basename = if windows_separators {
        name.rsplit(['/', '\\']).next()
    } else {
        name.rsplit('/').next()
    }
    .filter(|name| !name.is_empty())
    .unwrap_or(name);
    if QueueUpload::is_valid_filename(basename) {
        basename.to_owned()
    } else {
        format!("file-{ordinal}.bin")
    }
}

fn validate_compound_document(
    document: &QueuePayloadDocument,
    capture: &QueueLocalCapture,
) -> Result<(), QueueCaptureFailure> {
    let QueuePayloadItem::Manifest(item) = document.item() else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let expected = compound_manifest(
        capture,
        item.attachment_manifest
            .iter()
            .map(|file| file.attachment.clone())
            .collect(),
    )?;
    let expected = QueuePayloadDocument::from_manifest(
        capture.directory(),
        expected,
        QueuePayloadUse::Enqueue,
        sha256,
    )?;
    if document.directory() != capture.directory() || document.bytes() != expected.bytes() {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn finish_compound_preparation(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &CapturedServer,
    source: &EnqueueSourceCapture,
    input: PreparedComposerInput,
    capture: Option<ComposerCapture>,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    let mut files = Vec::with_capacity(input.files.len());
    if input.files.len() != input.capture.item().files.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    for (ordinal, (descriptor, file)) in input
        .files
        .iter()
        .zip(&input.capture.item().files)
        .enumerate()
    {
        if file.byte_len != descriptor.byte_len() || &file.sha256 != descriptor.sha256() {
            return Err(QueueCaptureFailure::Invalid);
        }
        let read = this
            .update_in(cx, |workspace, _, _| {
                if !compound_allowed(workspace, id, server)
                    || workspace
                        .attachments
                        .get(descriptor.id().as_str())
                        .is_none_or(|held| held.blob != *descriptor)
                {
                    return None;
                }
                workspace
                    .attachment_worker()
                    .map(|worker| worker.read_attachment(*descriptor))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let bytes = read
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        if !this
            .update_in(cx, |workspace, _, _| {
                compound_allowed(workspace, id, server)
            })
            .unwrap_or(false)
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = server.owner.clone();
        let filename = upload_filename(&file.name, ordinal);
        let media_type = file.mime_type.clone();
        let digest = *descriptor.sha256();
        let upload = NetworkTask::spawn_on(&server.executor, async move {
            let owned = bytes.as_slice().to_vec();
            drop(bytes);
            QueueUpload::new(
                owner,
                &format!("file-{ordinal}"),
                &filename,
                &media_type,
                digest,
                owned,
            )?
            .submit()
            .await
        });
        files.push(upload.await.ok_or(QueueCaptureFailure::Unavailable)??);
    }
    let directory = server.owner.scope().directory().to_owned();
    let generation = input.generation;
    let revision = input.revision;
    let document = cx
        .background_executor()
        .spawn(async move {
            let manifest = compound_manifest(&input.capture, files)?;
            QueuePayloadDocument::from_manifest(
                &directory,
                manifest,
                QueuePayloadUse::Enqueue,
                sha256,
            )
            .map_err(QueueCaptureFailure::from)
        })
        .await?;
    let (mut document, bytes) = cx
        .background_executor()
        .spawn(async move {
            let bytes = document.bytes().to_vec();
            (document, bytes)
        })
        .await;
    let payload = persist_compound(
        this,
        cx,
        id,
        "payload",
        QueueObjectKind::Payload,
        bytes,
        false,
        activation,
    )
    .await?;
    let mut chunks = Vec::new();
    for ordinal in 0..document.chunk_count() {
        if !this
            .update_in(cx, |workspace, _, _| {
                compound_allowed(workspace, id, server)
            })
            .unwrap_or(false)
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = server.owner.clone();
        let upload = NetworkTask::spawn_on(&server.executor, async move {
            let descriptor = document
                .chunk_upload(owner, ordinal, sha256)?
                .submit()
                .await?;
            Ok::<_, openchamber_native_transport::queue_manifest::QueueManifestError>((
                document, descriptor,
            ))
        });
        let (returned, descriptor) = upload.await.ok_or(QueueCaptureFailure::Unavailable)??;
        document = returned;
        chunks.push(descriptor);
    }
    let owner = server.owner.clone();
    let prepared = cx
        .background_executor()
        .spawn(async move {
            QueuePayloadEnqueue::new(owner, document.pointer(chunks, sha256)?, &document, sha256)
        })
        .await?;
    let request = QueueJson::new(prepared.body().to_vec())?;
    let capture = match capture {
        Some(capture) => capture,
        None => ComposerCapture::from_published_at(
            source.composer.clone(),
            source.intent.edit.clone(),
            generation,
            revision,
        )?,
    };
    this.update_in(cx, |workspace, window, cx| {
        if !compound_allowed(workspace, id, server) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let previous = workspace.queue_storage.clone();
        workspace.change_queue_storage(|queue, workspace| {
            queue.prepare_enqueue(id, payload, request, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })?;
        // Payload and prepared inputs are already durable. The document owner
        // consumes only this exact revision and preserves newer input and Undo.
        if let Err(error) = workspace.consume_composer_submission(id, &capture, window, cx) {
            workspace.queue_storage = previous;
            return Err(error);
        }
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await
}

async fn source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<QueueNativeSource, QueueCaptureFailure> {
    let retained = this
        .update_in(cx, |this, _, _| {
            let operation = this.queue_storage.operation(id)?;
            match operation.action() {
                QueueAction::Enqueue { source, .. } => Some(source.clone()),
                _ => None,
            }
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let source = match retained {
        QueueSource::Inline(source) => *source,
        QueueSource::Companion(object) => {
            let bytes = queue_payload_io::read(this, cx, &object).await?;
            cx.background_executor()
                .spawn(async move {
                    serde_json::from_slice::<QueueNativeSource>(&bytes)
                        .map_err(|_| QueueCaptureFailure::Invalid)
                })
                .await?
        }
    };
    let valid = this
        .update_in(cx, |this, _, _| {
            this.queue_storage.operation(id).is_some_and(|operation| {
                operation
                    .owner()
                    .draft_key()
                    .as_ref()
                    .is_ok_and(|owner| owner == &source.owner)
            })
        })
        .unwrap_or(false);
    if !valid {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(source)
}

fn mention(mention: &Mention) -> QueueMention {
    QueueMention {
        start: u64::from(mention.start),
        end: u64::from(mention.end),
        text: mention.text.clone(),
    }
}

/// Converts semantic content without consulting catalogs or allocating new context IDs.
fn manifest(
    source: &QueueNativeSource,
    configuration: QueuedSendConfig,
    files: Vec<QueueAttachmentDescriptor>,
) -> Result<QueueManifestItem, QueueCaptureFailure> {
    let content = &source.draft.content;
    if files.len() != content.files.len()
        || files
            .iter()
            .zip(&content.files)
            .enumerate()
            .any(|(ordinal, (upload, file))| {
                file.name
                    .as_deref()
                    .is_none_or(|name| upload.filename != upload_filename(name, ordinal))
                    || file.byte_len != Some(upload.size)
                    || file.media_type.as_deref() != Some(upload.mime_type.as_str())
            })
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let metadata = |value: Option<&str>| {
        value
            .map(|value| {
                serde_json::from_str(value)
                    .map(openchamber_native_transport::prompt_parts::Metadata)
                    .map_err(|_| QueueCaptureFailure::Invalid)
            })
            .transpose()
    };
    let mut context = Vec::new();
    for entry in &content.context {
        // Native draft context identities are semantic chip IDs, not admission IDs.
        // The enqueue receipt freezes each resulting instruction/context identity.
        if let Some(instructions) = &entry.instructions {
            context.push(QueuePromptContext {
                id: None,
                text: instructions.clone(),
                description: None,
                metadata: None,
            });
        }
        if !entry.text.trim().is_empty() {
            context.push(QueuePromptContext {
                id: None,
                text: entry.text.clone(),
                description: entry.description.clone(),
                metadata: metadata(entry.metadata_json.as_deref())?,
            });
        } else if entry.description.is_some() || entry.metadata_json.is_some() {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    Ok(QueueManifestItem {
        content: source.draft.text.clone(),
        text: source.draft.text.clone(),
        attachment_manifest: files
            .into_iter()
            .zip(&content.files)
            .map(|(attachment, file)| QueueManifestAttachment {
                agent_path: file
                    .name
                    .as_ref()
                    .filter(|name| *name != &attachment.filename)
                    .cloned(),
                attachment,
                description: None,
                mention: None,
            })
            .collect(),
        context: Vec::new(),
        context_preview: None,
        send_config: configuration,
        prompt_parts: QueuePromptParts {
            file_references: Vec::new(),
            agents: content
                .agents
                .iter()
                .map(|agent| QueueAgent {
                    name: agent.name.clone(),
                    mention: agent.mention.as_ref().map(mention),
                })
                .collect(),
            skills: content
                .skills
                .iter()
                .map(|skill| QueueSkill {
                    id: skill.id.clone(),
                    mention: None,
                })
                .collect(),
            metadata: metadata(content.metadata_json.as_deref())?,
            context,
        },
    })
}

async fn prepare(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &CapturedServer,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    let native = source(this, cx, id).await?;
    let native = cx
        .background_executor()
        .spawn(async move {
            let bytes = serde_json::to_vec(&native).map_err(|_| QueueCaptureFailure::Invalid)?;
            Ok::<_, QueueCaptureFailure>((native, bytes))
        })
        .await?;
    let (native, bytes) = native;
    if activation == Activation::Retry {
        queue_payload_io::persist_retry(
            this,
            cx,
            id,
            "source",
            QueueObjectKind::NativeCompanion,
            bytes,
            true,
        )
        .await?;
    } else {
        queue_payload_io::persist(
            this,
            cx,
            id,
            "source",
            QueueObjectKind::NativeCompanion,
            bytes,
            true,
        )
        .await?;
    }
    let configuration = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .operation(id)
                .map(|operation| operation.configuration().restore())
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let mut files = Vec::new();
    for (ordinal, file) in native.draft.content.files.iter().enumerate() {
        let read = this
            .update_in(cx, |this, _, _| {
                if this.close_in_progress() {
                    return None;
                }
                let descriptor = this.attachments.get(&file.id)?.blob;
                if file.byte_len != Some(descriptor.byte_len()) {
                    return None;
                }
                this.attachment_worker()
                    .map(|worker| worker.read_attachment(descriptor))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let bytes = read
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        let filename = upload_filename(
            file.name.as_deref().ok_or(QueueCaptureFailure::Invalid)?,
            ordinal,
        );
        let media_type = file
            .media_type
            .clone()
            .ok_or(QueueCaptureFailure::Invalid)?;
        let owner = server.owner.clone();
        if !dispatch_allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let owned = bytes.as_slice().to_vec();
            drop(bytes);
            QueueUpload::new(
                owner,
                &format!("file-{ordinal}"),
                &filename,
                &media_type,
                sha256(&owned),
                owned,
            )?
            .submit()
            .await
        });
        files.push(request.await.ok_or(QueueCaptureFailure::Unavailable)??);
    }
    let directory = server.owner.scope().directory().to_owned();
    let (native, document) = cx
        .background_executor()
        .spawn(async move {
            let item = manifest(&native, configuration, files)?;
            let document = QueuePayloadDocument::from_manifest(
                &directory,
                item,
                QueuePayloadUse::Enqueue,
                sha256,
            )?;
            Ok::<_, QueueCaptureFailure>((native, document))
        })
        .await?;
    let (mut document, bytes) = cx
        .background_executor()
        .spawn(async move {
            let bytes = document.bytes().to_vec();
            (document, bytes)
        })
        .await;
    let payload = if activation == Activation::Retry {
        queue_payload_io::persist_retry(
            this,
            cx,
            id,
            "payload",
            QueueObjectKind::Payload,
            bytes,
            false,
        )
        .await?
    } else {
        queue_payload_io::persist(
            this,
            cx,
            id,
            "payload",
            QueueObjectKind::Payload,
            bytes,
            false,
        )
        .await?
    };
    let mut chunks = Vec::new();
    for ordinal in 0..document.chunk_count() {
        if !dispatch_allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = server.owner.clone();
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let upload = document.chunk_upload(owner, ordinal, sha256)?;
            let descriptor = upload.submit().await?;
            Ok::<_, openchamber_native_transport::queue_manifest::QueueManifestError>((
                document, descriptor,
            ))
        });
        let (returned, descriptor) = request.await.ok_or(QueueCaptureFailure::Unavailable)??;
        document = returned;
        chunks.push(descriptor);
    }
    let owner = server.owner.clone();
    let capture = cx
        .background_executor()
        .spawn(async move {
            let pointer = document.pointer(chunks, sha256)?;
            QueuePayloadEnqueue::new(owner, pointer, &document, sha256)
        })
        .await?;
    let request = QueueJson::new(capture.body().to_vec())?;
    this.update_in(cx, |this, window, cx| {
        if this.close_in_progress() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.change_queue_storage(|queue, workspace| {
            queue.prepare_enqueue(id, payload, request, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })?;
        // The source companion is already durable. Consumption removes only
        // this matching capture, preserving newer typing and attachment owners.
        this.consume_draft_capture(&native.owner, &native.draft, window, cx);
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await
}

fn dispatch_allowed(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> bool {
    this.update_in(cx, |this, _, _| {
        !this.close_in_progress()
            && this.persistence_ready()
            && this.queue_storage.operation(id).is_some()
    })
    .unwrap_or(false)
}

async fn enqueue(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &CapturedServer,
    activation: Activation,
) -> Result<EnqueueCheck, QueueCaptureFailure> {
    // This barrier also makes explicit recovery usable after a prior failed save.
    queue_payload_io::save(this, cx).await?;
    let capturing = this
        .update_in(cx, |this, _, _| {
            this.queue_storage.operation(id).is_some_and(|operation| {
                matches!(
                    operation.action(),
                    QueueAction::Enqueue {
                        phase: QueueEnqueuePhase::Capturing,
                        ..
                    }
                )
            })
        })
        .unwrap_or(false);
    if capturing {
        if activation == Activation::Check {
            return Ok(EnqueueCheck::Unknown);
        }
        let compound = this
            .update_in(cx, |workspace, _, _| {
                workspace
                    .queue_storage
                    .operation(id)
                    .is_some_and(QueueOperation::has_compound_source)
            })
            .unwrap_or(false);
        if compound {
            if activation != Activation::Retry {
                return Err(QueueCaptureFailure::Unavailable);
            }
            retry_compound_preparation(this, cx, id, server).await?;
        } else {
            prepare(this, cx, id, server, activation).await?;
        }
    }
    let (payload, body, accepted, pending_retry) = this
        .update_in(cx, |this, _, _| {
            match this.queue_storage.operation(id)?.action() {
                QueueAction::Enqueue {
                    phase:
                        QueueEnqueuePhase::Ready { payload, request }
                        | QueueEnqueuePhase::Unknown { payload, request },
                    ..
                } => Some((payload.clone(), request.clone(), None, None)),
                QueueAction::Enqueue {
                    phase:
                        QueueEnqueuePhase::Accepted {
                            payload,
                            request,
                            receipt,
                        },
                    ..
                } => Some((
                    payload.clone(),
                    request.clone(),
                    Some(receipt.clone()),
                    None,
                )),
                QueueAction::Enqueue {
                    phase:
                        QueueEnqueuePhase::RetryPrepared {
                            payload,
                            request,
                            receipt,
                            retry,
                        },
                    ..
                } => Some((
                    payload.clone(),
                    request.clone(),
                    Some(receipt.clone()),
                    Some(retry.clone()),
                )),
                _ => None,
            }
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let (capture, observed) = if accepted.is_some() {
        // Accepted server custody is independently observable when the local
        // payload is missing or corrupt. The immutable receipt is compared below.
        if !dispatch_allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = server.owner.clone();
        let request = NetworkTask::spawn_on(&server.executor, async move {
            match owner.lookup(None).await? {
                QueueLookup::Found(record) => match *record {
                    QueueRecord::Enqueue(record) => Ok(Some(record)),
                    _ => Err(openchamber_native_transport::queue_manifest::QueueManifestError::ReceiptMismatch),
                },
                QueueLookup::Unknown => Ok(None),
            }
        });
        (
            None,
            request.await.ok_or(QueueCaptureFailure::Unavailable)??,
        )
    } else {
        let bytes = queue_payload_io::read(this, cx, &payload).await?;
        let compound = this
            .update_in(cx, |workspace, _, _| {
                workspace
                    .queue_storage
                    .operation(id)
                    .is_some_and(QueueOperation::has_compound_source)
            })
            .unwrap_or(false);
        let inputs = if compound {
            Some(composer_payload::restore(this, cx, id).await?)
        } else {
            None
        };
        let owner = server.owner.clone();
        let directory = owner.scope().directory().to_owned();
        let capture = cx
            .background_executor()
            .spawn(async move {
                let document = QueuePayloadDocument::from_bytes(
                    bytes,
                    &directory,
                    QueuePayloadUse::Enqueue,
                    sha256,
                )?;
                if let Some(inputs) = inputs {
                    validate_compound_document(&document, &inputs.capture)?;
                }
                QueuePayloadEnqueue::from_body(owner, body.bytes().to_vec(), &document, sha256)
                    .map_err(QueueCaptureFailure::from)
            })
            .await?;
        if !dispatch_allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let lookup = activation != Activation::Initial;
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let observed = if lookup {
                capture.lookup().await?
            } else {
                None
            };
            Ok::<_, openchamber_native_transport::queue_manifest::QueueManifestError>((
                capture, observed,
            ))
        });
        let (capture, observed) = request.await.ok_or(QueueCaptureFailure::Unavailable)??;
        (Some(capture), observed)
    };
    let was_observed = observed.is_some();
    let mut record = if let Some(record) = observed {
        record
    } else {
        // An observed receipt can never become fresh enqueue authority, even
        // when a later lookup reports it missing.
        if accepted.is_some() {
            return Err(QueueCaptureFailure::Invalid);
        }
        if activation == Activation::Check {
            return Ok(EnqueueCheck::NotObserved);
        }
        let capture = capture.ok_or(QueueCaptureFailure::Invalid)?;
        // Unknown is committed before POST, including the first explicit attempt.
        this.update_in(cx, |this, _, _| this.queue_storage.mark_unknown(id))
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
        if !dispatch_allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let request =
            NetworkTask::spawn_on(&server.executor, async move { capture.submit().await });
        request.await.ok_or(QueueCaptureFailure::Unavailable)??
    };
    if let Some(accepted) = &accepted
        && accepted.value()?.get("receipt") != receipt(&record)?.value()?.get("receipt")
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let pending_attempt = pending_retry
        .as_ref()
        .map(|retry| {
            retry
                .value()?
                .get("attempt")
                .and_then(serde_json::Value::as_u64)
                .ok_or(QueueCaptureFailure::Invalid)
        })
        .transpose()?;
    let unissued_retry = pending_attempt.is_some_and(|attempt| attempt > record.attempt)
        && record.state == QueueEnqueueState::Uncertain;
    if activation == Activation::Check && unissued_retry {
        return Ok(EnqueueCheck::Unknown);
    }
    if activation == Activation::Retry
        && was_observed
        && record.state == QueueEnqueueState::Uncertain
        && (pending_attempt.is_none() || unissued_retry)
    {
        let attempt = pending_attempt.unwrap_or(
            record
                .attempt
                .checked_add(1)
                .ok_or(QueueCaptureFailure::Invalid)?,
        );
        let retry = QueueRetry::new(server.owner.clone(), record.receipt.clone(), attempt)?;
        let body = QueueJson::new(retry.body().to_vec())?;
        if let Some(pending) = pending_retry {
            if pending != body {
                return Err(QueueCaptureFailure::Invalid);
            }
        } else {
            let observed = receipt(&record)?;
            this.update_in(cx, |this, _, _| {
                if this.close_in_progress() {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                this.change_queue_storage(|queue, workspace| {
                    queue.record_enqueue_accepted(id, observed, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })?;
                    queue.prepare_enqueue_retry(id, body, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })?;
                Ok::<_, QueueCaptureFailure>(())
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        }
        queue_payload_io::save(this, cx).await?;
        if !dispatch_allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let request = NetworkTask::spawn_on(&server.executor, async move { retry.submit().await });
        record = request.await.ok_or(QueueCaptureFailure::Unavailable)??;
    }
    let receipt = receipt(&record)?;
    // A receipt first accepted here is this window's own evidence that the
    // message was queued. A later observation of the same receipt is not.
    let first_acceptance = accepted.is_none();
    this.update_in(cx, |this, _, _| {
        if this.close_in_progress() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.change_queue_storage(|queue, workspace| {
            queue.record_enqueue_accepted(id, receipt, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })?;
        this.queue
            .enqueues
            .deliveries
            .insert(id.clone(), record.state);
        if first_acceptance {
            // The reread below answers for this queue.
            this.queue.enqueues.await_listing(id.clone());
            if record.state == QueueEnqueueState::Queued {
                this.queue.enqueues.note_listed(id.clone());
            }
        }
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    let _ = this.update_in(cx, |this, _, cx| {
        if this.runtime == server.runtime {
            this.read_queue_owner(
                openchamber_native_core::message_queue::queue_owner(
                    server.owner.scope().directory(),
                )
                .map(str::to_owned),
                cx,
            );
        }
    });
    if matches!(
        record.state,
        QueueEnqueueState::Dispatched
            | QueueEnqueueState::Removed
            | QueueEnqueueState::Taken
            | QueueEnqueueState::Deleted
    ) {
        retire(this, cx, id).await?;
    }
    Ok(EnqueueCheck::Unknown)
}

pub(crate) async fn retire(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<(), QueueCaptureFailure> {
    let compound = this
        .update_in(cx, |workspace, _, _| {
            workspace
                .queue_storage
                .operation(id)
                .is_some_and(QueueOperation::has_compound_source)
        })
        .unwrap_or(false);
    if !compound {
        return retire_with_attachments(this, cx, id, &[]).await;
    }
    let plan = this
        .update_in(cx, |workspace, _, _| {
            prepare_compound_retirement(workspace, id)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    release_retirement(this, cx, plan).await
}

/// Consumer, unrooted editing versions, original Takes and attachment intents
/// enter one snapshot. There is no intermediate crash state that loses cleanup
/// obligations after removing the consumer's source or preparation records.
fn prepare_compound_retirement(
    workspace: &mut Workspace,
    id: &QueueOperationId,
) -> Result<QueueRetirement, QueueCaptureFailure> {
    if !workspace.persistence_ready() || workspace.close_in_progress() {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let operation = workspace
        .queue_storage
        .operation(id)
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let owner = operation.owner().draft_key()?;
    let dependencies = operation
        .dependencies()
        .iter()
        .map(|reference| reference.operation.clone())
        .collect::<Vec<_>>();
    let files = workspace.queue_storage.composer_file_descriptors(id)?;
    workspace.release_composer_submission(id);
    let previous_queue = workspace.queue_storage.clone();
    let previous_attachments = workspace.attachments.clone();
    let previous_retired = workspace.retired_attachment_blobs.clone();
    let result = (|| {
        let mut plan = prepare_retirement(workspace, id, &[])?;
        let old_chunks: HashSet<_> = workspace
            .queue_storage
            .retired_blobs()?
            .into_iter()
            .collect();
        workspace.change_queue_storage(|queue, workspace| {
            queue.retire_composer_edits(&owner, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })?;
        plan.chunks.extend(
            workspace
                .queue_storage
                .retired_blobs()?
                .into_iter()
                .filter(|blob| !old_chunks.contains(blob)),
        );
        for dependency in dependencies {
            // Current input, Undo and independent consumers keep their Takes.
            let mut candidate = workspace.queue_storage.clone();
            if candidate
                .retire_operation(&dependency, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
                .is_err()
            {
                continue;
            }
            let take = prepare_retirement(workspace, &dependency, &[])?;
            plan.chunks.extend(take.chunks);
            plan.attachments.extend(take.attachments);
        }
        for file in files {
            let id = AttachmentId::from(file.id.clone());
            if workspace.attachment_has_owner(&id) {
                continue;
            }
            let descriptor = BlobDescriptor::new(
                BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?,
                file.byte_len,
                file.sha256,
            );
            if workspace
                .attachments
                .get(&id)
                .is_some_and(|held| held.blob != descriptor)
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            match attachment_retirement::begin_retirement(
                &mut workspace.attachments,
                &mut workspace.retired_attachment_blobs,
                &id,
                *descriptor.id(),
            ) {
                RetirementStart::Ready => plan.attachments.push(*descriptor.id()),
                RetirementStart::NotOwned => return Err(QueueCaptureFailure::Invalid),
                RetirementStart::AtCapacity => return Err(QueueCaptureFailure::Invalid),
            }
        }
        plan.chunks
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        plan.chunks.dedup();
        plan.attachments
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        plan.attachments.dedup();
        workspace.validate_queue_candidate(&workspace.queue_storage)?;
        Ok(plan)
    })();
    if result.is_err() {
        workspace.queue_storage = previous_queue;
        workspace.attachments = previous_attachments;
        workspace.retired_attachment_blobs = previous_retired;
    }
    result
}

/// Operation removal, catalog removal and attachment intents share one full
/// snapshot. A failed capacity/preflight restores every original owner.
pub(crate) fn prepare_retirement(
    workspace: &mut Workspace,
    id: &QueueOperationId,
    files: &[AttachmentId],
) -> Result<QueueRetirement, JournalError> {
    let bindings = workspace
        .queue_storage
        .local_take(id)
        .map(|local| {
            local
                .transfer
                .items
                .iter()
                .flat_map(|item| &item.files)
                .map(|file| file.descriptor())
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let owned: Vec<_> = bindings
        .iter()
        .map(|descriptor| AttachmentId::from(descriptor.id().as_str()))
        .collect();
    let send = workspace
        .queue_storage
        .operation(id)
        .is_some_and(|operation| {
            matches!(
                operation.action(),
                QueueAction::Take {
                    purpose: QueueTakePurpose::Send { .. },
                    ..
                }
            )
        });
    let files = if files.is_empty() && send {
        owned.as_slice()
    } else {
        files
    };
    if !files.is_empty()
        && (files.iter().any(|file| !owned.contains(file))
            || owned.iter().any(|file| !files.contains(file)))
    {
        return Err(JournalError::Invalid(
            "attachment retirement differs from its complete take bindings",
        ));
    }
    let chunks = workspace
        .queue_storage
        .objects()
        .iter()
        .filter(|object| object.operation() == id)
        .flat_map(|object| object.reference().chunks())
        .map(|chunk| chunk.descriptor().map(|descriptor| *descriptor.id()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut queue = workspace.queue_storage.clone();
    queue.retire_operation(id, |_| Ok(()))?;
    let mut attachments = workspace.attachments.clone();
    let mut retired = workspace.retired_attachment_blobs.clone();
    let mut releases = Vec::new();
    for file in files {
        if queue.references_native(file)
            || queue.references_captured_native(file)
            || attachment_retirement::referenced(
                file,
                &workspace.drafts,
                &workspace.send_attempts,
                &workspace.creations,
                &workspace.starters.captures,
                &workspace.attachment_preparations,
            )
        {
            continue;
        }
        let blob = BlobId::parse(file.as_str())
            .map_err(|_| JournalError::Invalid("invalid queue attachment retirement identity"))?;
        if attachments
            .get(file)
            .is_some_and(|prepared| !bindings.contains(&prepared.blob))
        {
            return Err(JournalError::Invalid(
                "queue attachment retirement changed its original descriptor",
            ));
        }
        match attachment_retirement::begin_retirement(&mut attachments, &mut retired, file, blob) {
            RetirementStart::Ready => {
                if !releases.contains(&blob) {
                    releases.push(blob);
                }
            }
            RetirementStart::NotOwned => {
                return Err(JournalError::Invalid(
                    "queue attachment lacks catalog or retirement ownership",
                ));
            }
            RetirementStart::AtCapacity => {
                return Err(JournalError::Invalid(
                    "attachment retirement capacity exceeded",
                ));
            }
        }
    }
    let previous_queue = std::mem::replace(&mut workspace.queue_storage, queue);
    let previous_attachments = std::mem::replace(&mut workspace.attachments, attachments);
    let previous_retired = std::mem::replace(&mut workspace.retired_attachment_blobs, retired);
    if let Err(error) = workspace.validate_queue_candidate(&workspace.queue_storage) {
        workspace.queue_storage = previous_queue;
        workspace.attachments = previous_attachments;
        workspace.retired_attachment_blobs = previous_retired;
        return Err(error);
    }
    Ok(QueueRetirement {
        chunks,
        attachments: releases,
    })
}

pub(crate) async fn retire_with_attachments(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    files: &[AttachmentId],
) -> Result<(), QueueCaptureFailure> {
    let plan = this
        .update_in(cx, |this, _, _| {
            if this.close_in_progress() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            prepare_retirement(this, id, files).map_err(QueueCaptureFailure::from)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    release_retirement(this, cx, plan).await
}

pub(crate) async fn release_retirement(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    plan: QueueRetirement,
) -> Result<(), QueueCaptureFailure> {
    // This single barrier commits both namespaces, including when close begins
    // before any release. A crash cannot lose attachment IDs with the operation.
    queue_payload_io::save(this, cx).await?;
    let mut failed = false;
    let releases = plan
        .chunks
        .into_iter()
        .map(RetiredByte::Queue)
        .chain(plan.attachments.into_iter().map(RetiredByte::Attachment));
    for entry in releases {
        let receipt = this
            .update_in(cx, |this, _, _| {
                if this.close_in_progress() {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                let worker = this
                    .attachment_worker()
                    .ok_or(QueueCaptureFailure::Unavailable)?;
                let receipt = match entry {
                    RetiredByte::Queue(blob) => worker.release_queue_chunk(blob),
                    RetiredByte::Attachment(blob) => {
                        if this.attachment_storage.owns_retirement(&blob)
                            || this.attachment_retirement.busy(&blob)
                            || this.attachment_has_owner(&AttachmentId::from(blob.as_str()))
                            || this.attachments.contains_key(blob.as_str())
                        {
                            return Ok(None);
                        }
                        worker.release_attachment(blob)
                    }
                };
                Ok(Some(receipt))
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        let Some(receipt) = receipt else {
            failed = true;
            continue;
        };
        let commit = match receipt
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
        {
            Ok(commit) => commit,
            Err(
                BlobFailure::Journal(_)
                | BlobFailure::Attachment(AttachmentFailure::NoDurableAuthority),
            ) => {
                return Err(QueueCaptureFailure::Persistence);
            }
            Err(BlobFailure::Attachment(_)) => {
                failed = true;
                continue;
            }
        };
        if this
            .update_in(cx, |this, _, _| match entry {
                RetiredByte::Queue(blob) => this.queue_storage.record_released(&blob, commit),
                RetiredByte::Attachment(blob) => {
                    if commit != SpoolCommit::Durable
                        || this.attachment_storage.owns_retirement(&blob)
                        || this.attachment_retirement.busy(&blob)
                        || this.attachment_has_owner(&AttachmentId::from(blob.as_str()))
                        || this.attachments.contains_key(blob.as_str())
                        || !this.retired_attachment_blobs.contains(&blob)
                    {
                        return Err(JournalError::Invalid(
                            "attachment release lacks durable unowned intent",
                        ));
                    }
                    this.retired_attachment_blobs
                        .retain(|retired| retired != &blob);
                    Ok(())
                }
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)?
            .is_err()
        {
            failed = true;
            continue;
        }
        queue_payload_io::save(this, cx).await?;
    }
    if failed {
        Err(QueueCaptureFailure::Persistence)
    } else {
        Ok(())
    }
}

fn receipt(record: &QueueEnqueueRecord) -> Result<QueueJson, QueueCaptureFailure> {
    let saved = &record.receipt;
    let identity = &saved.identity;
    let digest = identity
        .request_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let state = match record.state {
        QueueEnqueueState::Queued => "queued",
        QueueEnqueueState::Uncertain => "uncertain",
        QueueEnqueueState::Dispatched => "dispatched",
        QueueEnqueueState::Removed => "removed",
        QueueEnqueueState::Taken => "taken",
        QueueEnqueueState::Deleted => "deleted",
    };
    let body = serde_json::json!({ "receipt": {
        "version": 1, "kind": "enqueue", "requestId": identity.request_id,
        "sessionId": identity.session_id.as_str(), "directory": identity.directory,
        "requestHash": digest, "acceptedAt": identity.accepted_at,
        "itemId": saved.item_id.as_str(), "messageId": saved.message_id.as_str(),
        "contextIds": saved.context_ids.iter().map(|id| id.as_str()).collect::<Vec<_>>()
    }, "state": state, "revision": record.revision, "attempt": record.attempt });
    QueueJson::new(serde_json::to_vec(&body).map_err(|_| QueueCaptureFailure::Invalid)?)
        .map_err(Into::into)
}

#[cfg(test)]
#[path = "queue_enqueue_tests.rs"]
mod tests;
