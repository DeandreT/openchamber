//! Reservation-first payload prefix sends. Hydration starts no work.
//!
//! The submit captures the source-aware editing revision, ordered companion owners,
//! native byte descriptors, configuration and identities before its first await. Preview
//! grants no reservation authority. Every outgoing byte and its full local source
//! are durable before an explicit reserve; only its matching receipt permits send.

use std::collections::{HashMap, HashSet};

use gpui_kit::{AsyncWindowContext, Context, Task, WeakEntity, Window};
use openchamber_native_core::draft::{Draft, DraftTarget};
use openchamber_native_core::draft_content::DraftFileContent;
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::{
    QueueKey, QueueSession, QueuedItemId, QueuedMessage, QueuedSendConfig,
};
use openchamber_native_core::pending_requests::{ReplyFailure, RequestId, RequestKey};
use openchamber_native_transport::prompt::{Delivery, SendObservation};
use openchamber_native_transport::prompt_parts::Metadata;
use openchamber_native_transport::queue_frozen_send::{
    QueueFrozenItem, QueueFrozenSend, QueueFrozenSource, QueueLocalFrozenItem, QueueLocalSendInput,
    QueuePreparedFile,
};
use openchamber_native_transport::queue_manifest::{
    QueueAgent, QueueAttachmentDescriptor, QueueAttachmentHandle, QueueIndexedPayload,
    QueueLocalCapture, QueueLocalFile, QueueLocalItem, QueueManifestAttachment, QueueManifestItem,
    QueueMention, QueuePayloadDocument, QueuePayloadFileSource, QueuePayloadPrefix,
    QueuePayloadPrefixCapture, QueuePayloadPrefixInput, QueuePayloadUse, QueuePromptContext,
    QueuePromptParts, QueueSkill, QueueUpload,
};
use serde_json::Value;

use crate::attachment_catalog::{
    AttachmentCatalog, PreparationHold, PreparedAttachment, StagingPhase,
};
use crate::attachment_spool::{BlobDescriptor, BlobId, SpoolCommit};
use crate::composer_document::{self, ComposerCapture};
use crate::composer_payload::{self, PreparedComposerInput};
use crate::connection::ConnectionState;
use crate::draft_companion::{self, CompanionCapture};
use crate::network::NetworkTask;
use crate::queue_capture::{
    QueueJson, QueueObjectId, QueueObjectKind, QueueOperationId, QueueSpoolRef, sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    NativePrefixIntent, NativePrefixKind, PrefixSourceCapture, QueueAction, QueueConfiguration,
    QueueFileBinding, QueueNativeFileDescriptor, QueueObject, QueueOperation, QueueOwner,
    QueuePrefixHandoff, QueuePrefixPhase, QueueSource, QueueStorage, QueueTransferItem,
};
use crate::queue_take::{self, MaterializedQueueItem, QueueServer};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PrefixCheck {
    #[default]
    Unknown,
    Working,
    Missing,
    Admitted,
    OutOfOrder,
    Failed,
}

struct PrefixTask {
    runtime: RuntimeId,
    session: SessionId,
    native_roots: HashSet<AttachmentId>,
    _task: Task<()>,
}

#[derive(Default)]
pub(crate) struct PrefixState {
    tasks: HashMap<QueueOperationId, PrefixTask>,
    checks: HashMap<QueueOperationId, PrefixCheck>,
    pending_inputs: HashMap<QueueOperationId, Vec<u8>>,
}

impl PrefixState {
    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.tasks.values().any(|task| &task.runtime == runtime)
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
    pub(crate) fn check(&self, id: &QueueOperationId) -> PrefixCheck {
        self.checks.get(id).copied().unwrap_or_default()
    }
    /// Pre-admission roots only prevent retirement while the exact editing
    /// snapshot flushes. They grant no spool-write or acknowledgement authority.
    pub(crate) fn references_native(&self, id: &AttachmentId) -> bool {
        self.tasks
            .values()
            .any(|task| task.native_roots.contains(id))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activation {
    Initial,
    Check,
    Retry,
}

enum Preparation {
    Ready,
    Refused,
}

struct InitialPrefix {
    capture: ComposerCapture,
    configuration: QueuedSendConfig,
    message: MessageId,
    item_ids: Vec<QueuedItemId>,
    delivery: Option<Delivery>,
    native_context_ids: Vec<MessageId>,
    skill_context_ids: Vec<MessageId>,
    files: Vec<QueueNativeFileDescriptor>,
}

struct CapturedPrefix {
    capture: ComposerCapture,
    skill_context_ids: Vec<MessageId>,
}

#[derive(Debug, PartialEq, Eq)]
enum RecoveryStep {
    Admitted,
    CheckMissing,
    OutOfOrder,
    Blocked,
    Reserve(Vec<MessageId>),
    Dispatch(Vec<MessageId>),
}

fn recovery_step(
    phase: &QueuePrefixPhase,
    activation: Activation,
    observed: SendObservation,
) -> RecoveryStep {
    if matches!(phase, QueuePrefixPhase::Capturing) {
        return RecoveryStep::Blocked;
    }
    match observed {
        SendObservation::Pending(_) | SendObservation::Delivered(_) => RecoveryStep::Admitted,
        SendObservation::OutOfOrder => RecoveryStep::OutOfOrder,
        SendObservation::Missing { .. } if activation == Activation::Check => {
            RecoveryStep::CheckMissing
        }
        SendObservation::Missing { admitted } => {
            if matches!(phase, QueuePrefixPhase::Reserved(_)) {
                RecoveryStep::Dispatch(admitted)
            } else {
                RecoveryStep::Reserve(admitted)
            }
        }
    }
}

/// A large inline reference is still eligible for payload transfer. Unsupported
/// goals, scheduled messages and the active sending item remain FIFO barriers.
pub(crate) fn leading_ids(
    queue: &[QueuedMessage],
    sending: Option<&QueuedItemId>,
) -> Vec<QueuedItemId> {
    if sending.is_some() {
        return Vec::new();
    }
    queue
        .iter()
        .take_while(|item| item.payload_takeable())
        .map(|item| item.id.clone())
        .collect()
}

impl Workspace {
    pub(crate) fn payload_prefix_blocks_session(
        &self,
        runtime: &RuntimeId,
        session: &SessionId,
    ) -> bool {
        self.queue.prefixes.tasks.values().any(|task| &task.runtime == runtime && &task.session == session)
            || self.queue_storage.operations().iter().any(|operation| operation.owner().runtime() == runtime.as_str()
            && operation.owner().session() == session.as_str()
            && matches!(operation.action(), QueueAction::Prefix { phase, .. }
                if self.queue.prefixes.busy(operation.id()) || !matches!(phase, QueuePrefixPhase::Admitted(_))))
    }

    pub(crate) fn begin_payload_prefix(
        &mut self,
        key: QueueKey,
        ids: Vec<QueuedItemId>,
        delivery: Option<Delivery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty()
            || !self.persistence_ready()
            || self.close_in_progress()
            || self.queue_preview_pending(&key.session)
        {
            return;
        }
        let mutation = self.mutation_key(&key.session);
        if key.runtime != self.runtime
            || !self.prompt_allowed(&mutation)
            || !self.send_attempts.can_send(&mutation)
            || self.creations.get(&mutation).is_some()
        {
            return;
        }
        let Some(owner) = self.composer_key.clone().filter(|owner| {
            owner.runtime() == &key.runtime
                && owner.target() == &DraftTarget::Session(key.session.clone())
        }) else {
            return;
        };
        if self.attachment_storage.has_pending(&owner)
            || self
                .attachment_preparations
                .iter()
                .any(|hold| hold.owner == owner)
        {
            return;
        }
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
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        let Some(model) = selection.model else {
            return;
        };
        let configuration = QueuedSendConfig {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: model.variant.clone(),
            agent: selection.agent_name,
        };
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
        let Ok(server) = QueueServer::new(
            key.runtime.clone(),
            client,
            connection.network.handle(),
            key.session.clone(),
            &key.directory,
        ) else {
            return;
        };
        let mut files = Vec::new();
        for file in &capture.composer.native.draft.content.files {
            if !matches!(file.content, DraftFileContent::Prepared)
                || !self.prepared_attachment_ready(&owner, &file.id)
            {
                self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                return;
            }
            let Some(prepared) = self
                .attachments
                .get(&file.id)
                .filter(|prepared| file.byte_len == Some(prepared.blob.byte_len()))
            else {
                return;
            };
            files.push(QueueNativeFileDescriptor {
                id: file.id.to_string(),
                byte_len: prepared.blob.byte_len(),
                sha256: *prepared.blob.sha256(),
            });
        }
        let superseded = self.requests_for_send(&mutation);
        let message = self.message_ids.next().0;
        let mut native_context_ids = Vec::new();
        for context in &capture.composer.native.draft.content.context {
            if context.instructions.is_some() {
                native_context_ids.push(self.message_ids.next().0);
            }
            if !context.text.trim().is_empty() {
                native_context_ids.push(self.message_ids.next().0);
            }
        }
        let skill_context_ids: Vec<_> = (0..crate::mutation_journal::MAX_ENTRIES)
            .map(|_| self.message_ids.next().0)
            .collect();
        let effort = self.capture_user_effort(&model);
        let Ok(id) = QueueOperationId::new(self.message_ids.next().0.as_str()) else {
            return;
        };
        self.effort_preferences
            .retain_user_send(&key.runtime, &message, effort);
        self.spawn_prefix(
            id,
            server,
            Activation::Initial,
            Some(superseded),
            Some(InitialPrefix {
                capture,
                configuration,
                message,
                item_ids: ids,
                delivery,
                native_context_ids,
                skill_context_ids,
                files,
            }),
            window,
            cx,
        );
    }

    pub(crate) fn check_payload_prefix(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_prefix(id, Activation::Check, window, cx);
    }
    pub(crate) fn retry_payload_prefix(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_prefix(id, Activation::Retry, window, cx);
    }

    fn activate_prefix(
        &mut self,
        id: &QueueOperationId,
        activation: Activation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() || self.queue.prefixes.busy(id) {
            return;
        }
        let Some(operation) = self.queue_storage.operation(id) else {
            return;
        };
        if operation.owner().runtime() != self.runtime.as_str()
            || !matches!(operation.action(), QueueAction::Prefix { .. })
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
        let Ok(server) = QueueServer::new(
            self.runtime.clone(),
            client,
            connection.network.handle(),
            SessionId::from(operation.owner().session()),
            operation.owner().directory(),
        ) else {
            return;
        };
        let key = self.mutation_key(server.scope.session());
        if activation == Activation::Retry
            && (!self.prompt_allowed(&key)
                || self.payload_local_send_blocks_session(&key.runtime, &key.session))
        {
            return;
        }
        // Request decisions belong to this activation only. Neither a saved
        // prefix nor a Check action grants authority to repeat them.
        let superseded = (activation == Activation::Retry).then(|| self.requests_for_send(&key));
        self.spawn_prefix(id.clone(), server, activation, superseded, None, window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_prefix(
        &mut self,
        id: QueueOperationId,
        server: QueueServer,
        activation: Activation,
        superseded: Option<Vec<RequestKey>>,
        initial: Option<InitialPrefix>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(initial) = &initial {
            let registered = (|| {
                let queue_owner = QueueOwner::new(
                    &server.runtime,
                    server.scope.directory(),
                    server.scope.session(),
                    &initial.capture.composer.native.owner,
                )?;
                let operation_id = id.clone();
                let config = QueueConfiguration::capture(&initial.configuration);
                let message = initial.message.clone();
                let intent = NativePrefixIntent {
                    kind: NativePrefixKind::Capture,
                    directory: server.scope.directory().to_owned(),
                    id: message.to_string(),
                    item_ids: initial.item_ids.iter().map(ToString::to_string).collect(),
                    delivery: initial.delivery,
                    companions: initial.capture.composer.references.clone(),
                    native_context_ids: initial
                        .native_context_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    files: initial.files.clone(),
                    edit: None,
                };
                self.register_composer_submission(
                    id.clone(),
                    initial.capture.clone(),
                    initial.skill_context_ids.clone(),
                    move |capture| {
                        let mut intent = intent.clone();
                        intent.edit = capture.edit.clone();
                        let body = QueueJson::new(serde_json::to_vec(&intent).map_err(|_| {
                            crate::mutation_journal::JournalError::Invalid(
                                "invalid captured prefix intent",
                            )
                        })?)?;
                        let mut operation = QueueOperation::prefix(
                            operation_id.clone(),
                            queue_owner.clone(),
                            config.clone(),
                            message.clone(),
                            capture.composer.native.clone(),
                            body,
                        )?;
                        operation
                            .begin_composer_preparation(capture.generation, capture.revision)?;
                        Ok(operation)
                    },
                )
            })();
            if registered.is_err() {
                self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
                return;
            }
        }
        let task_id = id.clone();
        let runtime = server.runtime.clone();
        let session = server.scope.session().clone();
        let initial_message = initial.as_ref().map(|initial| initial.message.clone());
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
                        std::iter::once(file.id.clone())
                            .chain(file.source_document_id.iter().cloned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.queue
            .prefixes
            .checks
            .insert(id.clone(), PrefixCheck::Working);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = run(&this, cx, &id, &server, activation, superseded, initial).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if result.is_err() {
                    this.settle_failed_composer_submission(&id);
                }
                let roots = this
                    .queue
                    .prefixes
                    .tasks
                    .get_mut(&id)
                    .map(|task| std::mem::take(&mut task.native_roots).into_iter().collect())
                    .unwrap_or_default();
                this.queue.prefixes.tasks.remove(&id);
                // A departure held back while this transfer ran is noted now.
                cx.defer_in(window, |this, window, cx| {
                    this.reconcile_enqueue_listings(window, cx);
                });
                this.retire_released_native_roots(roots, window, cx);
                if this.queue_storage.operation(&id).is_some() {
                    this.queue.prefixes.checks.insert(
                        id.clone(),
                        result.as_ref().copied().unwrap_or(PrefixCheck::Failed),
                    );
                } else {
                    this.queue.prefixes.checks.remove(&id);
                }
                if this.queue_storage.operation(&id).is_none()
                    && let Some(message) = &initial_message
                {
                    this.effort_preferences
                        .forget_user_send(&server.runtime, message);
                }
                if this.runtime == server.runtime && result.is_err() && !this.close_in_progress() {
                    this.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
                }
                this.resume_native_root_close(window, cx);
                cx.notify();
            });
        });
        self.queue.prefixes.tasks.insert(
            task_id,
            PrefixTask {
                runtime,
                session,
                native_roots,
                _task: task,
            },
        );
        cx.notify();
    }
}

fn supersession_complete(
    captured: &[RequestKey],
    current: &[RequestKey],
    same_runtime: bool,
    cancelled: bool,
) -> bool {
    cancelled && current.is_empty() && (captured.is_empty() || same_runtime)
}

/// Request keys are memory-only, captured by Initial or explicit Retry before
/// preparation. A later arrival can block dispatch but never enters this list.
async fn supersede_requests(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    captured: &[RequestKey],
) -> Result<(), QueueCaptureFailure> {
    let key = crate::send_attempt::SessionKey {
        runtime: server.runtime.clone(),
        session: server.scope.session().clone(),
    };
    let attempts = this
        .update_in(cx, |this, _, _| {
            if this.close_in_progress()
                || !this.persistence_ready()
                || this.queue_storage.operation(id).is_none()
                || (!captured.is_empty() && this.runtime != server.runtime)
                || (this.runtime == server.runtime && !this.prompt_allowed(&key))
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
            this.prepare_supersession(captured)
                .map_err(|_| QueueCaptureFailure::Unavailable)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let mut cancelled = true;
    for attempt in attempts {
        let request_key = attempt.key().clone();
        let request = request_key.clone();
        let client = server.client.clone();
        let reply = NetworkTask::spawn_on(&server.executor, async move {
            let decision = match request.id {
                RequestId::Permission(_) => crate::blocking_requests::RequestReply::Permission(
                    openchamber_native_core::permission::PermissionReply::Reject,
                ),
                RequestId::Form(_) => crate::blocking_requests::RequestReply::Dismiss,
            };
            crate::blocking_requests::respond(&client, &request, &decision).await
        });
        let result = reply.await;
        let success = matches!(
            result,
            Some(Ok(()))
                | Some(Err(
                    openchamber_native_transport::requests::RequestError::PermissionNotFound
                        | openchamber_native_transport::requests::RequestError::FormNotFound
                        | openchamber_native_transport::requests::RequestError::FormAlreadySettled
                ))
        );
        cancelled &= success;
        this.update_in(cx, |this, window, cx| {
            if let Some(store) = this.blocking.stores.get_mut(&request_key.scope) {
                if success {
                    let _ = store.settle(&request_key);
                } else {
                    let _ = store.reply_failed(attempt, ReplyFailure::Unknown);
                    this.blocking.errors.insert(request_key);
                }
            }
            this.sync_request_drafts(window, cx);
            cx.notify();
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    }
    let complete = this
        .update_in(cx, |this, _, _| {
            !this.close_in_progress()
                && this.persistence_ready()
                && this.queue_storage.operation(id).is_some()
                && (this.runtime != server.runtime || this.prompt_allowed(&key))
                && supersession_complete(
                    captured,
                    &this.requests_for_send(&key),
                    this.runtime == server.runtime,
                    cancelled,
                )
        })
        .unwrap_or(false);
    if complete {
        Ok(())
    } else {
        Err(QueueCaptureFailure::Unavailable)
    }
}

fn operation(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<QueueOperation, QueueCaptureFailure> {
    this.update_in(cx, |this, _, _| this.queue_storage.operation(id).cloned())
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)
}

fn allowed(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> bool {
    this.update_in(cx, |this, _, _| {
        !this.close_in_progress()
            && this.persistence_ready()
            && this.composer_submission_dispatch_allowed(id)
            && this.queue_storage.operation(id).is_some()
    })
    .unwrap_or(false)
}

/// Local labels derive from the original random operation identity and a local
/// role. They never derive from server blob IDs, names, paths or user content.
fn blob_label(id: &QueueOperationId, role: &str) -> Result<BlobId, QueueCaptureFailure> {
    let digest = sha256(format!("native-prefix:{}:{role}", id.as_str()).as_bytes());
    BlobId::parse(
        &digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .map_err(|_| QueueCaptureFailure::Invalid)
}

#[allow(clippy::too_many_arguments)]
async fn persist(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    role: &str,
    kind: QueueObjectKind,
    bytes: Vec<u8>,
    source: bool,
    activation: Activation,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    if activation == Activation::Retry {
        queue_payload_io::persist_retry(this, cx, id, role, kind, bytes, source).await
    } else {
        queue_payload_io::persist(this, cx, id, role, kind, bytes, source).await
    }
}

async fn capture_initial(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    _server: &QueueServer,
    initial: InitialPrefix,
) -> Result<CapturedPrefix, QueueCaptureFailure> {
    let InitialPrefix {
        skill_context_ids, ..
    } = initial;
    let capture = composer_document::complete_submission(this, cx, id, false, None).await?;
    this.update_in(cx, |this, _, _| {
        if let Some(task) = this.queue.prefixes.tasks.get_mut(id) {
            task.native_roots.clear();
        }
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    Ok(CapturedPrefix {
        capture,
        skill_context_ids,
    })
}

async fn source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<PrefixSourceCapture, QueueCaptureFailure> {
    let retained = operation(this, cx, id)?;
    let QueueAction::Prefix {
        source, request, ..
    } = retained.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let source = match source {
        QueueSource::Inline(native) => {
            let intent: NativePrefixIntent = serde_json::from_slice(request.bytes())
                .map_err(|_| QueueCaptureFailure::Invalid)?;
            Ok(PrefixSourceCapture {
                version: 1,
                composer: CompanionCapture {
                    native: native.as_ref().clone(),
                    references: intent.companions.clone(),
                },
                intent,
            })
        }
        QueueSource::Companion(object) => {
            let bytes = queue_payload_io::read(this, cx, object).await?;
            cx.background_executor()
                .spawn(async move {
                    let source: PrefixSourceCapture =
                        serde_json::from_slice(&bytes).map_err(|_| QueueCaptureFailure::Invalid)?;
                    if source.version != 1 || source.composer.references != source.intent.companions
                    {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                    Ok(source)
                })
                .await
        }
    }?;
    if source.version != 1
        || source.composer.native.owner != retained.owner().draft_key()?
        || source.composer.references != source.intent.companions
        || source.composer.references != retained.dependencies()
        || source.intent.edit.as_ref() != retained.composer_edit()
        || source.intent.directory != retained.owner().directory()
        || source.intent.id
            != match retained.action() {
                QueueAction::Prefix { message, .. } => message.as_str(),
                _ => return Err(QueueCaptureFailure::Invalid),
            }
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    if let Some(expected) = retained.source_sha256() {
        let bytes = serde_json::to_vec(&source).map_err(|_| QueueCaptureFailure::Invalid)?;
        if &sha256(&bytes) != expected {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    Ok(source)
}

struct ComposerPlan {
    item: QueueLocalItem,
    context_ids: Vec<MessageId>,
    files: Vec<BlobDescriptor>,
}

fn metadata(value: Option<&str>) -> Result<Option<Metadata>, QueueCaptureFailure> {
    value
        .map(|value| {
            serde_json::from_str(value)
                .map(Metadata)
                .map_err(|_| QueueCaptureFailure::Invalid)
        })
        .transpose()
}

fn native_item(
    source: &PrefixSourceCapture,
    config: QueuedSendConfig,
) -> Result<ComposerPlan, QueueCaptureFailure> {
    let draft = &source.composer.native.draft;
    if draft.content.files.len() != source.intent.files.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    let mut files = Vec::new();
    let mut descriptors = Vec::new();
    for (file, descriptor) in draft.content.files.iter().zip(&source.intent.files) {
        let blob = BlobId::parse(&descriptor.id).map_err(|_| QueueCaptureFailure::Invalid)?;
        if file.id.as_str() != descriptor.id.as_str()
            || file.byte_len != Some(descriptor.byte_len)
            || !matches!(file.content, DraftFileContent::Prepared)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        descriptors.push(BlobDescriptor::new(
            blob,
            descriptor.byte_len,
            descriptor.sha256,
        ));
        files.push(QueueLocalFile {
            name: file.name.clone().ok_or(QueueCaptureFailure::Invalid)?,
            mime_type: file
                .media_type
                .clone()
                .ok_or(QueueCaptureFailure::Invalid)?,
            byte_len: descriptor.byte_len,
            sha256: descriptor.sha256,
            description: None,
            mention: None,
        });
    }
    let mut context = Vec::new();
    for entry in &draft.content.context {
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
    let mention = |value: &openchamber_native_core::draft_content::Mention| QueueMention {
        start: u64::from(value.start),
        end: u64::from(value.end),
        text: value.text.clone(),
    };
    if context.len() != source.intent.native_context_ids.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    for (context, id) in context.iter_mut().zip(&source.intent.native_context_ids) {
        context.id = Some(MessageId::from(id.clone()));
    }
    Ok(ComposerPlan {
        item: QueueLocalItem {
            content: draft.text.clone(),
            text: draft.text.clone(),
            files,
            send_config: config,
            prompt_parts: QueuePromptParts {
                file_references: Vec::new(),
                agents: draft
                    .content
                    .agents
                    .iter()
                    .map(|agent| QueueAgent {
                        name: agent.name.clone(),
                        mention: agent.mention.as_ref().map(mention),
                    })
                    .collect(),
                skills: draft
                    .content
                    .skills
                    .iter()
                    .map(|skill| QueueSkill {
                        id: skill.id.clone(),
                        mention: None,
                    })
                    .collect(),
                metadata: metadata(draft.content.metadata_json.as_deref())?,
                context,
            },
        },
        context_ids: source
            .intent
            .native_context_ids
            .iter()
            .cloned()
            .map(MessageId::from)
            .collect(),
        files: descriptors,
    })
}

fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right.get(key).is_some_and(|right| json_equal(value, right))
                })
        }
        _ => left == right,
    }
}

fn shifted(value: &mut Option<QueueMention>, offset: u64) -> Result<(), QueueCaptureFailure> {
    if let Some(value) = value {
        value.start = value
            .start
            .checked_add(offset)
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or(QueueCaptureFailure::Invalid)?;
        value.end = value
            .end
            .checked_add(offset)
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or(QueueCaptureFailure::Invalid)?;
    }
    Ok(())
}

fn append_item(
    plan: &mut ComposerPlan,
    mut item: QueueLocalItem,
    ids: &[MessageId],
    files: Vec<BlobDescriptor>,
) -> Result<(), QueueCaptureFailure> {
    if item.files.len() != files.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    let has_text = !plan.item.text.trim().is_empty();
    let incoming_text = !item.text.trim().is_empty();
    let offset = if has_text && incoming_text {
        plan.item.text.encode_utf16().count() as u64 + 2
    } else if has_text {
        plan.item.text.encode_utf16().count() as u64
    } else {
        0
    };
    for file in &mut item.files {
        shifted(&mut file.mention, offset)?;
    }
    for file in &mut item.prompt_parts.file_references {
        shifted(&mut file.mention, offset)?;
    }
    for agent in &mut item.prompt_parts.agents {
        shifted(&mut agent.mention, offset)?;
    }
    for skill in &mut item.prompt_parts.skills {
        shifted(&mut skill.mention, offset)?;
    }
    if has_text && incoming_text {
        plan.item.text.push_str("\n\n");
    }
    if incoming_text {
        if !has_text {
            plan.item.text.clear();
        }
        plan.item.text.push_str(&item.text);
    }
    if !plan.item.content.is_empty() && !item.content.is_empty() {
        plan.item.content.push_str("\n\n");
    }
    plan.item.content.push_str(&item.content);
    if let Some(incoming) = item.prompt_parts.metadata {
        let current = plan
            .item
            .prompt_parts
            .metadata
            .get_or_insert_with(Metadata::default);
        for (key, value) in incoming.0 {
            if current
                .0
                .get(&key)
                .is_some_and(|old| !json_equal(old, &value))
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            current.0.entry(key).or_insert(value);
        }
    }
    plan.item.files.extend(item.files);
    plan.files.extend(files);
    plan.item
        .prompt_parts
        .agents
        .extend(item.prompt_parts.agents);
    plan.item
        .prompt_parts
        .skills
        .extend(item.prompt_parts.skills);
    plan.item
        .prompt_parts
        .file_references
        .extend(item.prompt_parts.file_references);
    plan.item
        .prompt_parts
        .context
        .extend(item.prompt_parts.context);
    plan.context_ids.extend_from_slice(ids);
    Ok(())
}

fn composer_plan(
    source: &PrefixSourceCapture,
    configuration: QueuedSendConfig,
    companions: Vec<MaterializedQueueItem>,
) -> Result<ComposerPlan, QueueCaptureFailure> {
    let mut plan = native_item(source, configuration)?;
    for companion in companions {
        queue_take::validate_file_bindings(&companion)?;
        let files = match companion.document.item() {
            openchamber_native_transport::queue_manifest::QueuePayloadItem::Manifest(item) => item.attachment_manifest.iter().map(|file| {
                companion.retained.files.iter().find(|binding| matches!(binding, QueueFileBinding::Manifest { blob, .. } if blob == &file.attachment.blob_id))
                    .ok_or(QueueCaptureFailure::Invalid)?.descriptor().map_err(Into::into)
            }).collect::<Result<Vec<_>, QueueCaptureFailure>>()?,
            openchamber_native_transport::queue_manifest::QueuePayloadItem::Legacy(item) => (0..item.attachments.len()).map(|ordinal| {
                companion.retained.files.iter().find(|binding| matches!(binding, QueueFileBinding::Legacy { ordinal: index, .. } if *index == ordinal))
                    .ok_or(QueueCaptureFailure::Invalid)?.descriptor().map_err(Into::into)
            }).collect::<Result<Vec<_>, QueueCaptureFailure>>()?,
        };
        let local = QueueLocalCapture::from_document(
            companion.document,
            companion.indexed.index().context_ids.clone(),
            sha256,
        )?;
        append_item(&mut plan, local.item().clone(), local.context_ids(), files)?;
    }
    Ok(plan)
}

/// Shared preparation has already projected exact text and global anchors. A
/// second append would insert separators and shift every companion mention again.
fn prepared_plan(input: &PreparedComposerInput) -> ComposerPlan {
    ComposerPlan {
        item: input.capture.item().clone(),
        context_ids: input.capture.context_ids().to_vec(),
        files: input.files.clone(),
    }
}

fn unique_files(
    plan: &ComposerPlan,
) -> Result<(Vec<BlobDescriptor>, Vec<usize>), QueueCaptureFailure> {
    let mut files: Vec<BlobDescriptor> = Vec::new();
    let mut occurrences = Vec::new();
    for descriptor in &plan.files {
        let index = match files.iter().position(|file| file.id() == descriptor.id()) {
            Some(index) if files[index] == *descriptor => index,
            Some(_) => return Err(QueueCaptureFailure::Invalid),
            None => {
                files.push(*descriptor);
                files.len() - 1
            }
        };
        occurrences.push(index);
    }
    Ok((files, occurrences))
}

async fn upload_composer(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    plan: ComposerPlan,
    activation: Activation,
) -> Result<
    (
        QueuePayloadDocument,
        Vec<BlobDescriptor>,
        Vec<QueueFileBinding>,
    ),
    QueueCaptureFailure,
> {
    let (files, occurrences) = unique_files(&plan)?;
    let mut uploaded: Vec<QueueAttachmentDescriptor> = Vec::new();
    let mut bindings = Vec::new();
    for (ordinal, descriptor) in files.iter().enumerate() {
        let read = this
            .update_in(cx, |this, _, _| {
                if this.close_in_progress()
                    || this.attachments.get(descriptor.id().as_str())?.blob != *descriptor
                {
                    return None;
                }
                this.attachment_worker()
                    .map(|worker| worker.read_attachment(*descriptor))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let bytes = read
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        if !allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = server.scope.request(id.as_str())?;
        let index = occurrences
            .iter()
            .position(|index| *index == ordinal)
            .ok_or(QueueCaptureFailure::Invalid)?;
        let media_type = plan.item.files[index].mime_type.clone();
        let digest = *descriptor.sha256();
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let owned = bytes.as_slice().to_vec();
            drop(bytes);
            // Upload names are internal, safe single components. Each manifest
            // use separately retains its original model-facing identity.
            QueueUpload::new(
                owner,
                &format!("composer-file-{ordinal}"),
                &format!("composer-{ordinal}.bin"),
                &media_type,
                digest,
                owned,
            )?
            .submit()
            .await
        });
        let descriptor_on_server = request.await.ok_or(QueueCaptureFailure::Unavailable)??;
        bindings.push(QueueFileBinding::Manifest {
            blob: descriptor_on_server.blob_id,
            local: descriptor.id().as_str().to_owned(),
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
        });
        uploaded.push(descriptor_on_server);
    }
    let directory = server.scope.directory().to_owned();
    let (document, files, bytes) = cx
        .background_executor()
        .spawn(async move {
            let attachments = plan
                .item
                .files
                .into_iter()
                .zip(occurrences)
                .map(|(file, index)| QueueManifestAttachment {
                    agent_path: (uploaded[index].filename != file.name).then_some(file.name),
                    attachment: uploaded[index].clone(),
                    description: file.description,
                    mention: file.mention,
                })
                .collect();
            let item = QueueManifestItem {
                content: plan.item.content,
                text: plan.item.text,
                attachment_manifest: attachments,
                context: Vec::new(),
                context_preview: None,
                send_config: plan.item.send_config,
                prompt_parts: plan.item.prompt_parts,
            };
            let document = QueuePayloadDocument::from_manifest(
                &directory,
                item,
                QueuePayloadUse::Composer,
                sha256,
            )?;
            let bytes = document.bytes().to_vec();
            Ok::<_, QueueCaptureFailure>((document, plan.files, bytes))
        })
        .await?;
    // The real composer payload is immutable before its upload labels are used.
    persist(
        this,
        cx,
        id,
        "composer",
        QueueObjectKind::Payload,
        bytes,
        false,
        activation,
    )
    .await?;
    Ok((document, files, bindings))
}

fn captured_composer_bindings(
    files: &[BlobDescriptor],
    document: &QueuePayloadDocument,
) -> Result<Vec<QueueFileBinding>, QueueCaptureFailure> {
    let openchamber_native_transport::queue_manifest::QueuePayloadItem::Manifest(item) =
        document.item()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    if item.attachment_manifest.len() != files.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    let mut bindings = Vec::new();
    let mut roots = HashMap::new();
    for (file, local) in item.attachment_manifest.iter().zip(files) {
        if file.attachment.size != local.byte_len() || &file.attachment.sha256 != local.sha256() {
            return Err(QueueCaptureFailure::Invalid);
        }
        if let Some(previous) = roots.insert(file.attachment.blob_id, *local) {
            if previous != *local {
                return Err(QueueCaptureFailure::Invalid);
            }
        } else {
            bindings.push(QueueFileBinding::Manifest {
                blob: file.attachment.blob_id,
                local: local.id().as_str().to_owned(),
                byte_len: local.byte_len(),
                sha256: *local.sha256(),
            });
        }
    }
    Ok(bindings)
}

fn restore_composer_bytes(
    local_bytes: Vec<u8>,
    composer_bytes: Vec<u8>,
    directory: &str,
    pointer: &openchamber_native_transport::queue_manifest::QueuePayloadPointer,
    files: &[BlobDescriptor],
) -> Result<
    (
        QueueLocalCapture,
        QueuePayloadDocument,
        Vec<QueueFileBinding>,
    ),
    QueueCaptureFailure,
> {
    let local = QueueLocalCapture::from_bytes(local_bytes, directory, sha256)?;
    let document = QueuePayloadDocument::from_bytes(
        composer_bytes,
        directory,
        QueuePayloadUse::Composer,
        sha256,
    )?;
    let openchamber_native_transport::queue_manifest::QueuePayloadItem::Manifest(item) =
        document.item()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    if document.pointer(
        pointer
            .chunks()
            .iter()
            .map(|chunk| chunk.attachment.clone())
            .collect(),
        sha256,
    )? != *pointer
        || files.len() != local.item().files.len()
        || files
            .iter()
            .zip(&local.item().files)
            .any(|(held, file)| held.byte_len() != file.byte_len || held.sha256() != &file.sha256)
        || item.content != local.item().content
        || item.text != local.item().text
        || item.send_config != local.item().send_config
        || item.prompt_parts != local.item().prompt_parts
        || item.attachment_manifest.len() != local.item().files.len()
        || item
            .attachment_manifest
            .iter()
            .zip(&local.item().files)
            .any(|(uploaded, local)| {
                uploaded.attachment.mime_type != local.mime_type
                    || uploaded.description != local.description
                    || uploaded.mention != local.mention
                    || uploaded
                        .agent_path
                        .as_ref()
                        .is_some_and(|name| name != &local.name)
            })
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let bindings = captured_composer_bindings(files, &document)?;
    Ok((local, document, bindings))
}

/// Explicit recovery reuses the original composer bytes and original upload
/// descriptors. It never re-encodes the saved request or allocates replacement IDs.
async fn restore_composer_request(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    files: Vec<BlobDescriptor>,
    expected_local: Option<&QueueLocalCapture>,
    capture: QueuePayloadPrefixCapture,
) -> Result<
    (
        QueuePayloadPrefixCapture,
        QueueLocalCapture,
        Vec<BlobDescriptor>,
        Vec<QueueFileBinding>,
    ),
    QueueCaptureFailure,
> {
    let local = QueueObjectId::new(&format!("{}-local-composer", id.as_str()))?;
    let local_bytes = queue_payload_io::read(this, cx, &local).await?;
    if expected_local.is_some_and(|expected| expected.bytes() != local_bytes) {
        return Err(QueueCaptureFailure::Invalid);
    }
    let composer = QueueObjectId::new(&format!("{}-composer", id.as_str()))?;
    let composer_bytes = queue_payload_io::read(this, cx, &composer).await?;
    let directory = server.scope.directory().to_owned();
    let pointer = capture.input().composer.clone();
    let (local, mut document, files, bindings) = cx
        .background_executor()
        .spawn(async move {
            let (local, document, bindings) =
                restore_composer_bytes(local_bytes, composer_bytes, &directory, &pointer, &files)?;
            Ok::<_, QueueCaptureFailure>((local, document, files, bindings))
        })
        .await?;
    let original_files = match document.item() {
        openchamber_native_transport::queue_manifest::QueuePayloadItem::Manifest(item) => item
            .attachment_manifest
            .iter()
            .map(|file| file.attachment.clone())
            .collect::<Vec<_>>(),
        _ => return Err(QueueCaptureFailure::Invalid),
    };
    let mut uploaded = HashSet::new();
    for (original, descriptor) in original_files.into_iter().zip(&files) {
        if !uploaded.insert(original.blob_id) {
            continue;
        }
        if original.request_id != id.as_str() {
            return Err(QueueCaptureFailure::Invalid);
        }
        let read = this
            .update_in(cx, |this, _, _| {
                if this.close_in_progress()
                    || this.attachments.get(descriptor.id().as_str())?.blob != *descriptor
                {
                    return None;
                }
                this.attachment_worker()
                    .map(|worker| worker.read_attachment(*descriptor))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let bytes = read
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        let owner = server.scope.request(id.as_str())?;
        let expected = original.clone();
        if !allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let upload = NetworkTask::spawn_on(&server.executor, async move {
            let upload = QueueUpload::new(
                owner,
                &original.attachment_id,
                &original.filename,
                &original.mime_type,
                original.sha256,
                bytes.as_slice().to_vec(),
            )?;
            drop(bytes);
            upload.submit().await
        });
        if upload.await.ok_or(QueueCaptureFailure::Unavailable)?? != expected {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    for ordinal in 0..document.chunk_count() {
        let expected = capture
            .input()
            .composer
            .chunks()
            .get(ordinal)
            .ok_or(QueueCaptureFailure::Invalid)?
            .attachment
            .clone();
        let owner = server.scope.request(id.as_str())?;
        if !allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let upload = NetworkTask::spawn_on(&server.executor, async move {
            let receipt = document
                .chunk_upload(owner, ordinal, sha256)?
                .submit()
                .await?;
            Ok::<_, openchamber_native_transport::queue_manifest::QueueManifestError>((
                document, receipt,
            ))
        });
        let (returned, receipt) = upload.await.ok_or(QueueCaptureFailure::Unavailable)??;
        document = returned;
        if receipt != expected {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    Ok((capture, local, files, bindings))
}

async fn request_capture(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    mut composer: QueuePayloadDocument,
    intent: &NativePrefixIntent,
) -> Result<QueuePayloadPrefixCapture, QueueCaptureFailure> {
    let mut chunks = Vec::new();
    for ordinal in 0..composer.chunk_count() {
        if !allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = server.scope.request(id.as_str())?;
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let chunk = composer
                .chunk_upload(owner, ordinal, sha256)?
                .submit()
                .await?;
            Ok::<_, openchamber_native_transport::queue_manifest::QueueManifestError>((
                composer, chunk,
            ))
        });
        let (returned, chunk) = request.await.ok_or(QueueCaptureFailure::Unavailable)??;
        composer = returned;
        chunks.push(chunk);
    }
    let scope = server.scope.clone();
    let message = MessageId::from(intent.id.clone());
    let ids = intent
        .item_ids
        .iter()
        .cloned()
        .map(QueuedItemId::new)
        .collect();
    let steer = intent.delivery == Some(Delivery::Steer);
    let capture = cx
        .background_executor()
        .spawn(async move {
            let pointer = composer.pointer(chunks, sha256)?;
            QueuePayloadPrefixCapture::new(
                scope,
                QueuePayloadPrefixInput {
                    id: message,
                    item_ids: ids,
                    composer: pointer,
                    steer,
                },
                sha256,
            )
        })
        .await?;
    let body = QueueJson::new(capture.body().to_vec())?;
    this.update_in(cx, |this, _, _| {
        this.change_queue_storage(|queue, workspace| {
            queue.prepare_prefix_request(id, body, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(capture)
}

fn retain_preview_objects(
    workspace: &mut Workspace,
    id: &QueueOperationId,
    preview: &QueuePayloadPrefix,
) -> Result<Vec<QueueTransferItem>, QueueCaptureFailure> {
    let mut candidate = workspace.queue_storage.clone();
    let mut items = Vec::new();
    for (ordinal, item) in preview.items.iter().enumerate() {
        let object_id = QueueObjectId::new(&format!("{}-queued-{ordinal}", id.as_str()))?;
        let chunks = item
            .payload
            .pointer()
            .chunks()
            .iter()
            .enumerate()
            .map(|(chunk, value)| {
                Ok(BlobDescriptor::new(
                    blob_label(id, &format!("queued-{ordinal}-chunk-{chunk}"))?,
                    value.attachment.size,
                    value.attachment.sha256,
                ))
            })
            .collect::<Result<Vec<_>, QueueCaptureFailure>>()?;
        let reference = QueueSpoolRef::new(
            item.payload.pointer().total_bytes(),
            item.payload.pointer().sha256(),
            chunks,
            QueueObjectKind::Payload,
        )?;
        if let Some(existing) = candidate.object(&object_id) {
            if existing.operation() != id || existing.reference() != &reference {
                return Err(QueueCaptureFailure::Invalid);
            }
        } else {
            let object = QueueObject::planned(
                object_id.clone(),
                id.clone(),
                QueueObjectKind::Payload,
                1,
                reference,
            )?;
            candidate
                .admit_object(object, |_| Ok(()))
                .map_err(|(error, _)| error)?;
        }
        items.push(QueueTransferItem {
            item: item.id.to_string(),
            index: QueueJson::new(item.payload.body()?)?,
            payload: object_id,
            files: Vec::new(),
        });
    }
    workspace.validate_queue_candidate(&candidate)?;
    workspace.queue_storage = candidate;
    Ok(items)
}

async fn receive_preview_payloads(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    items: &[QueueTransferItem],
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    queue_payload_io::save(this, cx).await?;
    for item in items {
        let indexed =
            QueueIndexedPayload::from_body(item.index.bytes(), server.scope.directory(), sha256)?;
        for (ordinal, chunk) in indexed.pointer().chunks().iter().enumerate() {
            let hold = this
                .update_in(cx, |this, _, _| {
                    this.queue_storage
                        .object(&item.payload)
                        .and_then(|object| object.hold(ordinal))
                })
                .map_err(|_| QueueCaptureFailure::Unavailable)?;
            let Some(hold) = hold else {
                continue;
            };
            if !allowed(this, cx, id) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let handle =
                QueueAttachmentHandle::new(server.scope.clone(), chunk.attachment.clone())?;
            let request =
                NetworkTask::spawn_on(&server.executor, async move { handle.download().await });
            let bytes = request
                .await
                .ok_or(QueueCaptureFailure::Unavailable)??
                .into_bytes();
            let put = this
                .update_in(cx, |this, _, _| {
                    if this.close_in_progress() {
                        return None;
                    }
                    this.attachment_worker().map(|worker| {
                        if activation == Activation::Retry {
                            worker.resume_queue_chunk(hold.clone(), bytes)
                        } else {
                            worker.put_queue_chunk(hold.clone(), bytes)
                        }
                    })
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            let receipt = put
                .await
                .map_err(|_| QueueCaptureFailure::Persistence)?
                .map_err(|_| QueueCaptureFailure::Persistence)?;
            this.update_in(cx, |this, _, _| {
                this.queue_storage.record_chunk(&hold, &receipt)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
            queue_payload_io::save(this, cx).await?;
        }
        let verify = this
            .update_in(cx, |this, _, _| {
                let object = this.queue_storage.object(&item.payload)?;
                this.attachment_worker()
                    .map(|worker| worker.verify_queue_capture(object.verification()))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let proof = verify
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        this.update_in(cx, |this, _, _| this.queue_storage.record_verified(&proof))
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    Ok(())
}

async fn bind_preview_files(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    mut items: Vec<MaterializedQueueItem>,
) -> Result<Vec<MaterializedQueueItem>, QueueCaptureFailure> {
    let saved = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .prefix_downloads(id)
                .filter(|items| !items.is_empty())
                .map(|items| items.to_vec())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    let previously_bound = saved.is_some();
    let captured_id = id.clone();
    items = cx
        .background_executor()
        .spawn(async move {
            if let Some(saved) = saved {
                if saved.len() != items.len() {
                    return Err(QueueCaptureFailure::Invalid);
                }
                for (item, retained) in items.iter_mut().zip(saved) {
                    if item.retained.item != retained.item
                        || item.retained.index != retained.index
                        || item.retained.payload != retained.payload
                    {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                    item.retained = retained;
                    queue_take::validate_file_bindings(item)?;
                }
                return Ok(items);
            }
            for (ordinal, item) in items.iter_mut().enumerate() {
                let proof = item.document.files_proof(sha256)?;
                for (file_index, file) in proof.files().iter().enumerate() {
                    let local =
                        blob_label(&captured_id, &format!("queued-{ordinal}-file-{file_index}"))?
                            .as_str()
                            .to_owned();
                    item.retained.files.push(match file.source() {
                        QueuePayloadFileSource::Manifest(blob) => QueueFileBinding::Manifest {
                            blob: *blob,
                            local,
                            byte_len: file.byte_len(),
                            sha256: *file.sha256(),
                        },
                        QueuePayloadFileSource::Legacy(index) => QueueFileBinding::Legacy {
                            ordinal: *index,
                            local,
                            byte_len: file.byte_len(),
                            sha256: *file.sha256(),
                        },
                    });
                }
                queue_take::validate_file_bindings(item)?;
            }
            Ok::<_, QueueCaptureFailure>(items)
        })
        .await?;
    this.update_in(cx, |this, _, _| {
        let owner = this
            .queue_storage
            .operation(id)
            .ok_or(QueueCaptureFailure::Unavailable)?
            .owner()
            .draft_key()?;
        let previous = this.attachment_preparations.clone();
        let result = (|| {
            for item in &items {
                for file in &item.retained.files {
                    let descriptor = file.descriptor()?;
                    if this
                        .attachments
                        .get(descriptor.id().as_str())
                        .is_some_and(|file| file.blob != descriptor)
                    {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                    let hold = PreparationHold {
                        owner: owner.clone(),
                        id: AttachmentId::from(descriptor.id().as_str()),
                        generation: 1,
                        blob: *descriptor.id(),
                    };
                    if let Some(existing) = this
                        .attachment_preparations
                        .iter()
                        .find(|existing| existing.id == hold.id)
                    {
                        if existing != &hold {
                            return Err(QueueCaptureFailure::Invalid);
                        }
                    } else if previously_bound {
                        return Err(QueueCaptureFailure::Invalid);
                    } else {
                        this.attachment_preparations.push(hold);
                    }
                }
            }
            let captured = items.iter().map(|item| item.retained.clone()).collect();
            this.change_queue_storage(|queue, workspace| {
                queue.prepare_prefix_downloads(id, captured, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
            })?;
            Ok(())
        })();
        if result.is_err() {
            this.attachment_preparations = previous;
        }
        result
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(items)
}

async fn receive_preview_files(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    items: &[MaterializedQueueItem],
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    for item in items {
        for binding in &item.retained.files {
            let descriptor = binding.descriptor()?;
            // Retain this exact committed hold across the download, hash and put
            // waits. A restart cannot create a replacement generation or label.
            let hold = this
                .update_in(cx, |this, _, _| {
                    let operation = this.queue_storage.operation(id)?;
                    let owner = operation.owner().draft_key().ok()?;
                    if !this
                        .queue_storage
                        .prefix_downloads(id)?
                        .iter()
                        .any(|saved| saved == &item.retained)
                    {
                        return None;
                    }
                    this.attachment_preparations
                        .iter()
                        .find(|hold| {
                            hold.owner == owner
                                && hold.generation == 1
                                && hold.blob == *descriptor.id()
                                && hold.id.as_str() == descriptor.id().as_str()
                        })
                        .cloned()
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            let exists = this
                .update_in(cx, |this, _, _| {
                    this.attachments
                        .get(descriptor.id().as_str())
                        .is_some_and(|file| file.blob == descriptor)
                })
                .unwrap_or(false);
            if exists {
                continue;
            }
            let bytes = match (item.document.item(), binding) {
                (
                    openchamber_native_transport::queue_manifest::QueuePayloadItem::Manifest(item),
                    QueueFileBinding::Manifest { blob, .. },
                ) => {
                    let original = item
                        .attachment_manifest
                        .iter()
                        .find(|file| &file.attachment.blob_id == blob)
                        .ok_or(QueueCaptureFailure::Invalid)?;
                    if !allowed(this, cx, id) {
                        return Err(QueueCaptureFailure::Unavailable);
                    }
                    let handle = QueueAttachmentHandle::new(
                        server.scope.clone(),
                        original.attachment.clone(),
                    )?;
                    let read =
                        NetworkTask::spawn_on(
                            &server.executor,
                            async move { handle.download().await },
                        );
                    read.await
                        .ok_or(QueueCaptureFailure::Unavailable)??
                        .into_bytes()
                }
                (
                    openchamber_native_transport::queue_manifest::QueuePayloadItem::Legacy(item),
                    QueueFileBinding::Legacy { ordinal, .. },
                ) => {
                    let file = item
                        .attachments
                        .get(*ordinal)
                        .ok_or(QueueCaptureFailure::Invalid)?
                        .clone();
                    cx.background_executor()
                        .spawn(async move { file.bytes() })
                        .await?
                }
                _ => return Err(QueueCaptureFailure::Invalid),
            };
            let bytes = cx
                .background_executor()
                .spawn(async move {
                    if bytes.len() as u64 != descriptor.byte_len()
                        || sha256(&bytes) != *descriptor.sha256()
                    {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                    Ok(bytes)
                })
                .await?;
            let put = this
                .update_in(cx, |this, _, _| {
                    if this.close_in_progress() || !this.attachment_preparations.contains(&hold) {
                        return None;
                    }
                    this.attachment_worker().map(|worker| {
                        if activation == Activation::Retry {
                            worker.resume_attachment(hold.clone(), descriptor, bytes)
                        } else {
                            worker.put_downloaded_attachment(hold.clone(), descriptor, bytes)
                        }
                    })
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            let receipt = put
                .await
                .map_err(|_| QueueCaptureFailure::Persistence)?
                .map_err(|_| QueueCaptureFailure::Persistence)?;
            if receipt.commit != SpoolCommit::Durable || receipt.descriptor != descriptor {
                return Err(QueueCaptureFailure::Persistence);
            }
            this.update_in(cx, |this, _, _| {
                // Keep the named preparation as root until the complete prefix
                // handoff publishes all file bindings in one snapshot.
                let inserted = publish_preview_file(
                    &mut this.attachments,
                    &this.attachment_preparations,
                    &hold,
                    descriptor,
                )?;
                if let Err(error) = this.validate_queue_candidate(&this.queue_storage) {
                    if inserted {
                        this.attachments.remove(&hold.id);
                    }
                    return Err(error.into());
                }
                Ok::<_, QueueCaptureFailure>(())
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
            queue_payload_io::save(this, cx).await?;
        }
    }
    Ok(())
}

fn publish_preview_file(
    catalog: &mut AttachmentCatalog,
    preparations: &[PreparationHold],
    hold: &PreparationHold,
    descriptor: BlobDescriptor,
) -> Result<bool, QueueCaptureFailure> {
    if !preparations.contains(hold)
        || hold.blob != *descriptor.id()
        || hold.id.as_str() != descriptor.id().as_str()
    {
        return Err(QueueCaptureFailure::Unavailable);
    }
    if let Some(previous) = catalog.get(&hold.id) {
        return if previous.blob == descriptor {
            Ok(false)
        } else {
            Err(QueueCaptureFailure::Invalid)
        };
    }
    catalog.insert(
        hold.id.clone(),
        PreparedAttachment {
            blob: descriptor,
            staging: None,
        },
    );
    Ok(true)
}

fn local_bindings(
    workspace: &Workspace,
    server: &QueueServer,
    files: &[BlobDescriptor],
) -> Result<Vec<QueuePreparedFile>, QueueCaptureFailure> {
    files
        .iter()
        .enumerate()
        .map(|(ordinal, descriptor)| {
            let prepared = workspace
                .attachments
                .get(descriptor.id().as_str())
                .ok_or(QueueCaptureFailure::Invalid)?;
            let target = prepared
                .staging
                .as_ref()
                .filter(|target| {
                    target.phase == StagingPhase::Ready
                        && target.runtime == server.runtime
                        && target.directory == server.scope.directory()
                })
                .ok_or(QueueCaptureFailure::Invalid)?;
            if prepared.blob != *descriptor {
                return Err(QueueCaptureFailure::Invalid);
            }
            QueuePreparedFile::new_local(
                ordinal,
                descriptor.byte_len(),
                *descriptor.sha256(),
                &target.uri,
            )
            .map_err(|_| QueueCaptureFailure::Invalid)
        })
        .collect()
}

/// Compare immutable typed seals and IDs. Display previews never supply hash
/// material, and the server's changing queue revision is deliberately absent.
fn same_receipt(
    expected: &QueueJson,
    actual: &QueuePayloadPrefix,
) -> Result<(), QueueCaptureFailure> {
    if expected != &QueuePrefixHandoff::capture_receipt(actual)? {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

async fn prepare(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    activation: Activation,
    initial: Option<CapturedPrefix>,
) -> Result<Preparation, QueueCaptureFailure> {
    let source = source(this, cx, id).await?;
    let retained = operation(this, cx, id)?;
    let (source, source_bytes) = cx
        .background_executor()
        .spawn(async move {
            let bytes = serde_json::to_vec(&source).map_err(|_| QueueCaptureFailure::Invalid)?;
            Ok::<_, QueueCaptureFailure>((source, bytes))
        })
        .await?;
    persist(
        this,
        cx,
        id,
        "source",
        QueueObjectKind::NativeCompanion,
        source_bytes,
        true,
        activation,
    )
    .await?;
    let pending = this
        .update_in(cx, |workspace, _, _| {
            workspace.queue.prefixes.pending_inputs.get(id).cloned()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    let sealed = retained
        .composer_preparation()
        .is_some_and(|plan| plan.inputs_object().is_some());
    let (plan, local, submission, prepared) = if sealed || pending.is_some() {
        let input = if sealed {
            composer_payload::restore(this, cx, id).await?
        } else {
            if activation != Activation::Retry {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let bytes = pending.ok_or(QueueCaptureFailure::Unavailable)?;
            let directory = server.scope.directory().to_owned();
            let input = cx
                .background_executor()
                .spawn(async move { PreparedComposerInput::decode(&bytes, &directory) })
                .await?;
            composer_payload::persist_and_seal(this, cx, id, &input, true).await?;
            input
        };
        let submission = match initial {
            Some(initial) => initial.capture,
            None => ComposerCapture::from_published_at(
                source.composer.clone(),
                source.intent.edit.clone(),
                input.generation,
                input.revision,
            )?,
        };
        this.update_in(cx, |workspace, _, _| {
            workspace.queue.prefixes.pending_inputs.remove(id);
            workspace.release_composer_submission(id);
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
        (prepared_plan(&input), input.capture, submission, true)
    } else if let Some(initial) = initial {
        let CapturedPrefix {
            capture,
            skill_context_ids,
        } = initial;
        let inputs = composer_document::materialize(this, cx, &capture).await?;
        let input = crate::composer_delivery::ComposerDeliveryInput {
            inputs,
            configuration: retained.configuration().restore(),
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
        let delivery = match crate::composer_delivery::prepare_operation(
            this,
            cx,
            id,
            server,
            capture.generation,
            capture.revision,
            activation == Activation::Retry,
            input,
        )
        .await
        {
            Ok(delivery) => delivery,
            Err(crate::composer_delivery::ComposerDeliveryError::Capture(error)) => {
                return Err(error);
            }
            Err(_) => return Ok(Preparation::Refused),
        };
        let (local, files) = delivery.into_capture()?;
        let digest = *operation(this, cx, id)?
            .source_sha256()
            .ok_or(QueueCaptureFailure::Invalid)?;
        let input =
            PreparedComposerInput::new(local, files, capture.generation, capture.revision, digest)?;
        // Expanded text, exclusions, file references and derived roots become
        // immutable before any composer payload or preview can acquire authority.
        let input = cx
            .background_executor()
            .spawn(async move {
                let bytes = input.encode()?;
                Ok::<_, QueueCaptureFailure>((input, bytes))
            })
            .await?;
        let (input, bytes) = input;
        this.update_in(cx, |workspace, _, _| {
            workspace
                .queue
                .prefixes
                .pending_inputs
                .insert(id.clone(), bytes);
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
        composer_payload::persist_and_seal(this, cx, id, &input, activation == Activation::Retry)
            .await?;
        this.update_in(cx, |workspace, _, _| {
            workspace.queue.prefixes.pending_inputs.remove(id);
            workspace.release_composer_submission(id);
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
        (prepared_plan(&input), input.capture, capture, true)
    } else if retained.composer_preparation().is_some() {
        // A new-protocol capture never reruns mutable snippets or file reads after
        // restart. Missing or incomplete saved input has no preparation fallback.
        let input = composer_payload::restore(this, cx, id).await?;
        let submission = ComposerCapture::from_published_at(
            source.composer.clone(),
            source.intent.edit.clone(),
            input.generation,
            input.revision,
        )?;
        (prepared_plan(&input), input.capture, submission, true)
    } else {
        // Earlier v8 native captures have no preparation ledger. Their original
        // composition remains recoverable without running a new reprocessor.
        if source.intent.edit.is_some() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let companions = draft_companion::materialize(this, cx, &source.composer).await?;
        let captured_source = source.clone();
        let configuration = retained.configuration().restore();
        let directory = server.scope.directory().to_owned();
        let composed = cx
            .background_executor()
            .spawn(async move {
                let plan = composer_plan(&captured_source, configuration, companions)?;
                let local = QueueLocalCapture::new(
                    &directory,
                    plan.item.clone(),
                    plan.context_ids.clone(),
                    sha256,
                )?;
                Ok::<_, QueueCaptureFailure>((plan, local))
            })
            .await;
        let (plan, local) = match composed {
            Ok(value) => value,
            Err(_) => return Ok(Preparation::Refused),
        };
        let submission = ComposerCapture::from_published_at(source.composer.clone(), None, 1, 1)?;
        (plan, local, submission, false)
    };
    let existing = match retained.action() {
        QueueAction::Prefix { request, .. } if request.value()?.get("kind").is_some() => None,
        QueueAction::Prefix { request, .. } => Some(QueuePayloadPrefixCapture::from_body(
            server.scope.clone(),
            request.bytes().to_vec(),
            sha256,
        )?),
        _ => return Err(QueueCaptureFailure::Invalid),
    };
    let (capture, local, local_descriptors, composer_files) = if let Some(capture) = existing {
        restore_composer_request(
            this,
            cx,
            id,
            server,
            plan.files,
            prepared.then_some(&local),
            capture,
        )
        .await?
    } else {
        let bytes = local.bytes().to_vec();
        persist(
            this,
            cx,
            id,
            "local-composer",
            QueueObjectKind::NativeCompanion,
            bytes,
            false,
            activation,
        )
        .await?;
        let (composer, local_descriptors, composer_files) =
            upload_composer(this, cx, id, server, plan, activation).await?;
        let capture = request_capture(this, cx, id, server, composer, &source.intent).await?;
        (capture, local, local_descriptors, composer_files)
    };
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let read = NetworkTask::spawn_on(&server.executor, async move { capture.preview().await });
    let preview = read.await.ok_or(QueueCaptureFailure::Unavailable)??;
    let retained = this
        .update_in(cx, |this, _, _| retain_preview_objects(this, id, &preview))
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    receive_preview_payloads(this, cx, id, server, &retained, activation).await?;
    let captured_operation = operation(this, cx, id)?;
    let queued =
        queue_take::materialize_items(this, cx, captured_operation.owner(), &retained).await?;
    let mut queued = bind_preview_files(this, cx, id, queued).await?;
    receive_preview_files(this, cx, id, server, &queued, activation).await?;
    let composer_item = QueueTransferItem {
        item: source.intent.id.clone(),
        index: QueueJson::new(preview.composer.body()?)?,
        payload: QueueObjectId::new(&format!("{}-composer", id.as_str()))?,
        files: composer_files,
    };
    let composer = queue_take::materialize_item(
        this,
        cx,
        server.scope.directory(),
        &composer_item,
        QueuePayloadUse::Composer,
    )
    .await?;
    queued.push(composer);
    let prepared = queue_take::prepare_staged_files(this, cx, id, server, &queued).await?;
    let local_files = this
        .update_in(cx, |this, _, _| {
            local_bindings(this, server, &local_descriptors)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let composer = queued
        .pop()
        .expect("the materialized composer was appended after queued inputs");
    let mut queued_prepared = prepared;
    queued_prepared
        .pop()
        .expect("the final staged input belongs to the composer");
    let client = server.client.clone();
    let session = server.scope.session().clone();
    let directory = server.scope.directory().to_owned();
    let message = MessageId::from(source.intent.id.clone());
    let delivery = source.intent.delivery;
    let expected_contexts = preview.context_ids.clone();
    let captured = cx
        .background_executor()
        .spawn(async move {
            let mut inputs: Vec<_> = queued
                .iter()
                .zip(&queued_prepared)
                .map(|(item, files)| {
                    QueueFrozenSource::Queued(QueueFrozenItem {
                        document: &item.document,
                        indexed_payload: &item.indexed,
                        files,
                    })
                })
                .collect();
            inputs.push(QueueFrozenSource::Local(QueueLocalFrozenItem {
                capture: &local,
                files: &local_files,
            }));
            let frozen = QueueFrozenSend::capture_local(
                &client,
                QueueLocalSendInput {
                    session: &session,
                    directory: &directory,
                    message_id: &message,
                    selected_input: inputs.len() - 1,
                    sources: &inputs,
                    delivery,
                },
                sha256,
            )
            .map_err(|_| QueueCaptureFailure::Invalid)?;
            if frozen.context_ids() != expected_contexts {
                return Err(QueueCaptureFailure::Invalid);
            }
            Ok::<_, QueueCaptureFailure>((queued, frozen.into_bytes()))
        })
        .await;
    let (queued, frame) = match captured {
        Ok(value) => value,
        Err(_) => return Ok(Preparation::Refused),
    };
    let frame = persist(
        this,
        cx,
        id,
        "send",
        QueueObjectKind::FrozenSend,
        frame,
        false,
        activation,
    )
    .await?;
    let handoff = QueuePrefixHandoff {
        receipt: QueuePrefixHandoff::capture_receipt(&preview)?,
        items: queued.into_iter().map(|item| item.retained).collect(),
        composer: composer.retained.payload.clone(),
        prompt: frame,
        context_ids: preview
            .context_ids
            .iter()
            .map(ToString::to_string)
            .collect(),
        composer_item: Some(composer.retained),
    };
    this.update_in(cx, |this, _, _| {
        if this.close_in_progress() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let ids: HashSet<_> = handoff
            .items
            .iter()
            .flat_map(|item| &item.files)
            .map(|file| {
                file.descriptor()
                    .map(|descriptor| AttachmentId::from(descriptor.id().as_str()))
            })
            .collect::<Result<_, _>>()?;
        let previous = this.attachment_preparations.clone();
        this.attachment_preparations
            .retain(|hold| !ids.contains(&hold.id));
        let result = this.change_queue_storage(|queue, workspace| {
            queue.prepare_prefix(id, handoff, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        });
        if result.is_err() {
            this.attachment_preparations = previous;
        }
        result?;
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    this.update_in(cx, |this, window, cx| {
        if this.close_in_progress() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.consume_composer_submission(id, &submission, window, cx)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(Preparation::Ready)
}

fn observed_prefix_unavailable(
    session: &SessionId,
    directory: &str,
    ids: &[String],
    sessions: &[QueueSession],
) -> Result<bool, QueueCaptureFailure> {
    let owner = openchamber_native_core::message_queue::queue_owner(directory);
    let mut seen = HashSet::new();
    let mut seen_items = HashSet::new();
    if ids.is_empty()
        || openchamber_native_core::request_owner::normalize_directory(directory).is_none()
        || sessions.iter().any(|queue| {
            !seen.insert(&queue.session)
                || openchamber_native_core::request_owner::normalize_directory(&queue.directory)
                    .is_none()
                || queue
                    .items
                    .iter()
                    .any(|item| !seen_items.insert((&queue.session, &item.id)))
                || openchamber_native_core::message_queue::queue_owner(&queue.directory) != owner
        })
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let Some(queue) = sessions.iter().find(|queue| &queue.session == session) else {
        return Ok(true);
    };
    Ok(queue.directory != directory
        || queue.sending.is_some()
        || queue.items.len() < ids.len()
        || queue.items.iter().zip(ids).any(|(item, expected)| {
            item.id.as_str() != expected.as_str() || !item.payload_takeable()
        }))
}

/// One complete queue-owner read through the captured client. Projection state,
/// generic HTTP failures and failed/partial reads never establish unavailability.
async fn prefix_unavailable(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
) -> Result<bool, QueueCaptureFailure> {
    let source = source(this, cx, id).await?;
    let capturing = this
        .update_in(cx, |this, _, _| {
            !this.close_in_progress()
                && this.queue_storage.operation(id).is_some_and(|operation| {
                    matches!(
                        operation.action(),
                        QueueAction::Prefix {
                            phase: QueuePrefixPhase::Capturing,
                            ..
                        }
                    )
                })
        })
        .unwrap_or(false);
    if !capturing {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let client = server.client.clone();
    let owner = openchamber_native_core::message_queue::queue_owner(server.scope.directory())
        .map(str::to_owned);
    let request_owner = owner.clone();
    let read = NetworkTask::spawn_on(&server.executor, async move {
        client.message_queue(request_owner.as_deref()).await
    });
    let snapshot = read
        .await
        .ok_or(QueueCaptureFailure::Unavailable)?
        .map_err(|_| QueueCaptureFailure::Transport)?;
    let unavailable = observed_prefix_unavailable(
        server.scope.session(),
        server.scope.directory(),
        &source.intent.item_ids,
        &snapshot.sessions,
    )?;
    this.update_in(cx, |this, _, _| {
        if this.runtime == server.runtime {
            this.queue.projection.apply_snapshot(
                &server.runtime,
                owner.as_deref(),
                snapshot.revision,
                snapshot.sessions,
            );
        }
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    Ok(unavailable)
}

fn captured_resources(
    source: &PrefixSourceCapture,
    items: &[QueueTransferItem],
    composer: Option<&QueueTransferItem>,
) -> Result<HashMap<AttachmentId, BlobDescriptor>, QueueCaptureFailure> {
    let mut resources = source
        .intent
        .files
        .iter()
        .map(|file| {
            let blob = BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?;
            Ok((
                AttachmentId::from(file.id.clone()),
                BlobDescriptor::new(blob, file.byte_len, file.sha256),
            ))
        })
        .collect::<Result<HashMap<_, _>, QueueCaptureFailure>>()?;
    for file in items.iter().chain(composer).flat_map(|item| &item.files) {
        let descriptor = file.descriptor()?;
        let key = AttachmentId::from(descriptor.id().as_str());
        if resources
            .insert(key, descriptor)
            .is_some_and(|previous| previous != descriptor)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    Ok(resources)
}

fn restore_abandoned_native(
    current: Option<&Draft>,
    source: &PrefixSourceCapture,
    compound_present: bool,
) -> Result<Draft, QueueCaptureFailure> {
    // Exact rich edit versions require the compound owner's restoration hook.
    // Keeping the capture is safe while that hook is unavailable.
    if source.intent.edit.is_some() || compound_present {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let captured = &source.composer.native.draft;
    if let Some(current) = current.filter(|current| current.same_content(captured)) {
        return Ok(current.clone());
    }
    crate::send_attempt::restore_failed(current, captured).map_err(|_| QueueCaptureFailure::Invalid)
}

/// Which retained phases an abandon may return to the composer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AbandonScope {
    /// Only a prefix still capturing, before any server handoff.
    Capturing,
    /// Also a previewed prefix the server marked stale while it held no reservation.
    RefusedReservation,
}

#[cfg(test)]
fn prepare_abandon(
    this: &mut Workspace,
    id: &QueueOperationId,
    source: &PrefixSourceCapture,
    current: Option<&Draft>,
) -> Result<crate::queue_enqueue::QueueRetirement, QueueCaptureFailure> {
    prepare_abandon_in(this, id, source, current, AbandonScope::Capturing)
}

/// Returns a release plan only after the original owner's draft holds the entire
/// native capture. Any merge or snapshot refusal retains every previous owner.
fn prepare_abandon_in(
    this: &mut Workspace,
    id: &QueueOperationId,
    source: &PrefixSourceCapture,
    current: Option<&Draft>,
    scope: AbandonScope,
) -> Result<crate::queue_enqueue::QueueRetirement, QueueCaptureFailure> {
    if this.close_in_progress() {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let operation = this
        .queue_storage
        .operation(id)
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let abandonable = match operation.action() {
        QueueAction::Prefix {
            phase: QueuePrefixPhase::Capturing,
            ..
        } => true,
        QueueAction::Prefix {
            phase: QueuePrefixPhase::Previewed(_) | QueuePrefixPhase::ReservationUnknown(_),
            ..
        } => scope == AbandonScope::RefusedReservation,
        _ => false,
    };
    if !abandonable {
        return Err(QueueCaptureFailure::Invalid);
    }
    let owner = operation.owner().draft_key()?;
    if source.composer.native.owner != owner {
        return Err(QueueCaptureFailure::Invalid);
    }
    // Detached companions also need the compound owner's exact handback.
    // Their original Take roots remain protected by the retained prefix.
    let mut retained = 0;
    for reference in &this.queue_storage.draft_companions(&owner) {
        if source.composer.references.get(retained) == Some(reference) {
            retained += 1;
        }
    }
    if retained != source.composer.references.len() {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let compound_present = this.composer_documents.contains(&owner)
        || this.queue_storage.composer_edit(&owner).is_some();
    let restored = restore_abandoned_native(current, source, compound_present)?;
    let downloads = this
        .queue_storage
        .prefix_downloads(id)
        .ok_or(QueueCaptureFailure::Invalid)?
        .to_vec();
    let mut resources = captured_resources(source, &downloads, None)?;
    for file in this.queue_storage.composer_file_descriptors(id)? {
        let descriptor = BlobDescriptor::new(
            BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?,
            file.byte_len,
            file.sha256,
        );
        if resources
            .insert(AttachmentId::from(file.id), descriptor)
            .is_some_and(|previous| previous != descriptor)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    let mut holds = Vec::new();
    for binding in downloads.iter().flat_map(|item| &item.files) {
        let descriptor = binding.descriptor()?;
        let expected = PreparationHold {
            owner: owner.clone(),
            id: AttachmentId::from(descriptor.id().as_str()),
            generation: 1,
            blob: *descriptor.id(),
        };
        let hold = this
            .attachment_preparations
            .iter()
            .find(|hold| *hold == &expected)
            .cloned()
            .ok_or(QueueCaptureFailure::Invalid)?;
        holds.push(hold);
    }
    if let Some(preparation) = operation.composer_preparation() {
        for file in &preparation.files {
            let expected = file.hold(&owner)?;
            if let Some(hold) = this
                .attachment_preparations
                .iter()
                .find(|hold| *hold == &expected)
            {
                if !holds.contains(hold) {
                    holds.push(hold.clone());
                }
            } else if file.state == crate::queue_storage::QueueDerivedFileState::Proposed {
                return Err(QueueCaptureFailure::Invalid);
            }
        }
    }
    let chunks = this
        .queue_storage
        .objects()
        .iter()
        .filter(|object| object.operation() == id)
        .flat_map(|object| object.reference().chunks())
        .map(|chunk| chunk.descriptor().map(|descriptor| *descriptor.id()))
        .collect::<Result<Vec<_>, _>>()?;
    let previous_queue = this.queue_storage.clone();
    let previous_catalog = this.attachments.clone();
    let previous_drafts = this.drafts.clone();
    let previous_preparations = this.attachment_preparations.clone();
    let previous_retired = this.retired_attachment_blobs.clone();
    let result = (|| {
        this.drafts.save(owner.clone(), restored);
        this.attachment_preparations
            .retain(|hold| !holds.contains(hold));
        this.change_queue_storage(|queue, workspace| {
            let preflight =
                |candidate: &QueueStorage| workspace.validate_queue_candidate(candidate);
            match scope {
                AbandonScope::Capturing => queue.abandon_prefix(id, preflight),
                AbandonScope::RefusedReservation => queue.abandon_refused_prefix(id, preflight),
            }
        })?;
        let mut attachments = Vec::new();
        for (resource, descriptor) in &resources {
            if this.attachment_has_owner(resource) {
                continue;
            }
            if this
                .attachments
                .get(resource)
                .is_some_and(|prepared| prepared.blob != *descriptor)
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            match crate::attachment_retirement::begin_retirement(
                &mut this.attachments,
                &mut this.retired_attachment_blobs,
                resource,
                *descriptor.id(),
            ) {
                crate::attachment_retirement::RetirementStart::Ready => {}
                crate::attachment_retirement::RetirementStart::NotOwned
                    if holds.iter().any(|hold| &hold.id == resource) =>
                {
                    if this.retired_attachment_blobs.len() >= crate::mutation_journal::MAX_ENTRIES {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                    this.retired_attachment_blobs.push(*descriptor.id());
                }
                _ => return Err(QueueCaptureFailure::Invalid),
            }
            attachments.push(*descriptor.id());
        }
        this.validate_queue_candidate(&this.queue_storage)?;
        Ok(crate::queue_enqueue::QueueRetirement {
            chunks,
            attachments,
        })
    })();
    if result.is_err() {
        this.queue_storage = previous_queue;
        this.attachments = previous_catalog;
        this.drafts = previous_drafts;
        this.attachment_preparations = previous_preparations;
        this.retired_attachment_blobs = previous_retired;
    } else {
        this.effort_preferences
            .forget_user_send(owner.runtime(), &MessageId::from(source.intent.id.clone()));
        this.queue.prefixes.pending_inputs.remove(id);
        this.release_composer_submission(id);
    }
    result
}

/// Capturing has never attempted a reservation. A failed local preflight or an
/// authoritative unavailable snapshot permits this atomic local handback.
async fn abandon(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    abandon_in(this, cx, id, activation, AbandonScope::Capturing).await
}

async fn abandon_in(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    activation: Activation,
    scope: AbandonScope,
) -> Result<(), QueueCaptureFailure> {
    let source = source(this, cx, id).await?;
    let plan = this
        .update_in(cx, |this, window, cx| {
            let owner = &source.composer.native.owner;
            let current = if this.composer_key.as_ref() == Some(owner) {
                Some(this.composer_draft(cx))
            } else {
                this.drafts.get(owner).cloned()
            };
            let plan = prepare_abandon_in(this, id, &source, current.as_ref(), scope)?;
            if this.composer_key.as_ref() == Some(owner) {
                this.restore_composer_draft(window, cx);
            }
            Ok::<_, QueueCaptureFailure>(plan)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    // The restored native draft, dependency ownership and both retirement
    // namespaces share the save that must finish before any named byte release.
    if activation == Activation::Check {
        queue_payload_io::save(this, cx).await
    } else {
        crate::queue_enqueue::release_retirement(this, cx, plan).await
    }
}

async fn admitted(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    message: &MessageId,
) -> Result<(), QueueCaptureFailure> {
    this.update_in(cx, |this, _, _| {
        this.change_queue_storage(|queue, workspace| {
            queue.record_prefix_admitted(id, message, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    let retained = operation(this, cx, id)?;
    let runtime = RuntimeId::from(retained.owner().runtime());
    let _ = this.update_in(cx, |this, _, cx| {
        if let Some(effort) = this.effort_preferences.take_user_send(&runtime, message) {
            this.record_user_effort(effort, cx);
        }
    });
    Ok(())
}

async fn retire(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<(), QueueCaptureFailure> {
    let source = source(this, cx, id).await?;
    let retained = operation(this, cx, id)?;
    let QueueAction::Prefix {
        phase: QueuePrefixPhase::Admitted(handoff),
        ..
    } = retained.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let mut resources =
        captured_resources(&source, &handoff.items, handoff.composer_item.as_ref())?;
    let plan = this
        .update_in(cx, |this, _, _| {
            if this.close_in_progress() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            this.release_composer_submission(id);
            this.queue.prefixes.pending_inputs.remove(id);
            // Include removed companion files and derived document outputs before
            // removing any owner. The delivered file list can legitimately omit them.
            for file in this.queue_storage.composer_file_descriptors(id)? {
                let descriptor = BlobDescriptor::new(
                    BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?,
                    file.byte_len,
                    file.sha256,
                );
                if resources
                    .insert(AttachmentId::from(file.id), descriptor)
                    .is_some_and(|previous| previous != descriptor)
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
            }
            let previous_queue = this.queue_storage.clone();
            let previous_catalog = this.attachments.clone();
            let previous_retired = this.retired_attachment_blobs.clone();
            let retired_chunks: HashSet<_> = previous_queue.retired_blobs()?.into_iter().collect();
            let result = (|| {
                let mut plan = crate::queue_enqueue::prepare_retirement(this, id, &[])?;
                // One candidate removes the consumer, then its unrooted edit versions,
                // then original Takes. Current drafts, undo layouts and other live
                // consumers keep their own edit and Take roots throughout this change.
                this.change_queue_storage(|queue, workspace| {
                    queue.retire_composer_edits(&source.composer.native.owner, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })?;
                for reference in &source.composer.references {
                    // Another draft or captured send can still own this Take.
                    if this
                        .queue_storage
                        .companion_has_external_owner(&reference.operation)
                    {
                        continue;
                    }
                    crate::queue_enqueue::prepare_retirement(this, &reference.operation, &[])?;
                }
                for (resource, descriptor) in &resources {
                    if this.attachment_has_owner(resource) {
                        continue;
                    }
                    if this
                        .attachments
                        .get(resource)
                        .is_some_and(|prepared| prepared.blob != *descriptor)
                    {
                        return Err(crate::mutation_journal::JournalError::Invalid(
                            "prefix retirement changed its captured attachment",
                        ));
                    }
                    match crate::attachment_retirement::begin_retirement(
                        &mut this.attachments,
                        &mut this.retired_attachment_blobs,
                        resource,
                        *descriptor.id(),
                    ) {
                        crate::attachment_retirement::RetirementStart::Ready => {
                            plan.attachments.push(*descriptor.id());
                        }
                        crate::attachment_retirement::RetirementStart::NotOwned => {
                            return Err(crate::mutation_journal::JournalError::Invalid(
                                "prefix attachment lacks retirement ownership",
                            ));
                        }
                        crate::attachment_retirement::RetirementStart::AtCapacity => {
                            return Err(crate::mutation_journal::JournalError::Invalid(
                                "prefix attachment retirement capacity exceeded",
                            ));
                        }
                    }
                }
                plan.chunks = this
                    .queue_storage
                    .retired_blobs()?
                    .into_iter()
                    .filter(|chunk| !retired_chunks.contains(chunk))
                    .collect();
                this.validate_queue_candidate(&this.queue_storage)?;
                Ok(plan)
            })();
            if result.is_err() {
                this.queue_storage = previous_queue;
                this.attachments = previous_catalog;
                this.retired_attachment_blobs = previous_retired;
            }
            result.map_err(QueueCaptureFailure::from)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    crate::queue_enqueue::release_retirement(this, cx, plan).await
}

async fn run(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    activation: Activation,
    superseded: Option<Vec<RequestKey>>,
    initial: Option<InitialPrefix>,
) -> Result<PrefixCheck, QueueCaptureFailure> {
    let initial = match initial {
        Some(initial) => Some(capture_initial(this, cx, id, server, initial).await?),
        None if activation == Activation::Retry => {
            let capture = composer_document::ensure_submission_edit(this, cx, id, true).await?;
            match capture {
                Some(capture) => {
                    let skill_context_ids = this
                        .update_in(cx, |workspace, _, _| {
                            workspace.composer_submission_skill_ids(id)
                        })
                        .ok()
                        .flatten()
                        .ok_or(QueueCaptureFailure::Unavailable)?;
                    Some(CapturedPrefix {
                        capture,
                        skill_context_ids,
                    })
                }
                None => None,
            }
        }
        None => None,
    };
    queue_payload_io::save(this, cx).await?;
    let retained = operation(this, cx, id)?;
    if matches!(
        retained.action(),
        QueueAction::Prefix {
            phase: QueuePrefixPhase::Capturing,
            ..
        }
    ) {
        if activation == Activation::Check {
            if retained.composer_edit().is_some_and(|edit| {
                this.update_in(cx, |workspace, _, _| {
                    workspace
                        .queue_storage
                        .object(&edit.object)
                        .is_none_or(|object| !object.verified())
                })
                .unwrap_or(true)
            }) {
                return Ok(PrefixCheck::Unknown);
            }
            if prefix_unavailable(this, cx, id, server).await? {
                abandon(this, cx, id, activation).await?;
                return Ok(PrefixCheck::Missing);
            }
            return Ok(PrefixCheck::Unknown);
        }
        match prepare(this, cx, id, server, activation, initial).await {
            Ok(Preparation::Ready) => {}
            Ok(Preparation::Refused) => {
                abandon(this, cx, id, activation).await?;
                return Ok(PrefixCheck::Missing);
            }
            Err(error) => {
                if prefix_unavailable(this, cx, id, server)
                    .await
                    .is_ok_and(|unavailable| unavailable)
                {
                    abandon(this, cx, id, activation).await?;
                    return Ok(PrefixCheck::Missing);
                }
                return Err(error);
            }
        }
    }
    let retained = operation(this, cx, id)?;
    let QueueAction::Prefix {
        message,
        request,
        phase,
        ..
    } = retained.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let handoff = match phase {
        QueuePrefixPhase::Previewed(handoff)
        | QueuePrefixPhase::ReservationUnknown(handoff)
        | QueuePrefixPhase::Reserved(handoff) => handoff,
        QueuePrefixPhase::Admitted(_) => {
            if activation != Activation::Check {
                retire(this, cx, id).await?;
            }
            return Ok(PrefixCheck::Admitted);
        }
        QueuePrefixPhase::Capturing => return Err(QueueCaptureFailure::Invalid),
    };
    if activation != Activation::Initial
        && matches!(phase, QueuePrefixPhase::Previewed(_))
        && let Some(preparation) = retained.composer_preparation()
    {
        // A crash may follow the durable handoff but precede revision consumption.
        // Revisit only the saved compound capture; an already consumed source or
        // an independent newer document remains untouched.
        let source = source(this, cx, id).await?;
        let capture = ComposerCapture::from_published_at(
            source.composer.clone(),
            source.intent.edit,
            preparation.generation,
            preparation.revision,
        )?;
        this.update_in(cx, |this, window, cx| {
            let current = this
                .queue_storage
                .draft_companions(&capture.composer.native.owner);
            let mut next = 0;
            for reference in current {
                if capture.composer.references.get(next) == Some(&reference) {
                    next += 1;
                }
            }
            if next == capture.composer.references.len() {
                this.consume_composer_submission(id, &capture, window, cx)?;
            }
            Ok::<_, QueueCaptureFailure>(())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    let message = MessageId::from(message.clone());
    let frozen =
        queue_take::restore_frozen_send(this, cx, server, id, &handoff.prompt, &message).await?;
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let observe = NetworkTask::spawn_on(&server.executor, async move {
        let result = frozen.observe().await;
        (frozen, result)
    });
    let (frozen, observed) = observe.await.ok_or(QueueCaptureFailure::Unavailable)?;
    let step = recovery_step(
        phase,
        activation,
        observed.map_err(|_| QueueCaptureFailure::Transport)?,
    );
    let (contexts, reserve_required) = match step {
        RecoveryStep::Admitted => {
            admitted(this, cx, id, &message).await?;
            if activation != Activation::Check {
                retire(this, cx, id).await?;
            }
            return Ok(PrefixCheck::Admitted);
        }
        RecoveryStep::OutOfOrder => return Ok(PrefixCheck::OutOfOrder),
        RecoveryStep::CheckMissing => return Ok(PrefixCheck::Missing),
        RecoveryStep::Blocked => return Err(QueueCaptureFailure::Invalid),
        RecoveryStep::Reserve(admitted) => (admitted, true),
        RecoveryStep::Dispatch(admitted) => (admitted, false),
    };
    if reserve_required {
        let capture = QueuePayloadPrefixCapture::from_body(
            server.scope.clone(),
            request.bytes().to_vec(),
            sha256,
        )?;
        this.update_in(cx, |this, _, _| this.queue_storage.mark_unknown(id))
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
        if !allowed(this, cx, id) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let reserve =
            NetworkTask::spawn_on(&server.executor, async move { capture.reserve().await });
        let reserved = match reserve.await.ok_or(QueueCaptureFailure::Unavailable)? {
            Ok(reserved) => reserved,
            // The queue changed while the session held no reservation, so this
            // request reserved nothing. Any other refusal may follow a reservation
            // whose answer was lost, and stays uncertain.
            Err(openchamber_native_transport::queue_manifest::QueueManifestError::PrefixStale) => {
                abandon_in(this, cx, id, activation, AbandonScope::RefusedReservation).await?;
                return Ok(PrefixCheck::Missing);
            }
            Err(error) => return Err(error.into()),
        };
        same_receipt(&handoff.receipt, &reserved)?;
        let receipt = QueuePrefixHandoff::capture_receipt(&reserved)?;
        this.update_in(cx, |this, _, _| {
            this.change_queue_storage(|queue, workspace| {
                queue.record_prefix_reserved(id, receipt, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
            })
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    let superseded = superseded.ok_or(QueueCaptureFailure::Unavailable)?;
    supersede_requests(this, cx, id, server, &superseded).await?;
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let send = NetworkTask::spawn_on(
        &server.executor,
        async move { frozen.dispatch(&contexts).await },
    );
    send.await
        .ok_or(QueueCaptureFailure::Unavailable)?
        .map_err(|_| QueueCaptureFailure::Transport)?;
    admitted(this, cx, id, &message).await?;
    retire(this, cx, id).await?;
    Ok(PrefixCheck::Admitted)
}

#[cfg(test)]
#[path = "queue_prefix_tests.rs"]
mod tests;
