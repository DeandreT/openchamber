//! Ordinary rich composer sends, with immutable local inputs and explicit recovery.
//!
//! The actor owns its captured runtime through every wait. Source publication,
//! expanded inputs, staging targets, the frozen frame and Prepared ownership each
//! precede dispatch. Recovery reads those bytes; it never runs preparation again.
//! Request decisions and the initial effort token remain window-local.

use std::collections::{HashMap, HashSet};

use gpui_kit::{AsyncWindowContext, Context, Task, WeakEntity, Window};
use openchamber_native_core::draft::{DraftKey, DraftTarget};
use openchamber_native_core::draft_content::DraftFileContent;
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_core::pending_requests::{
    ReplyFailure, RequestAttempt, RequestId, RequestKey,
};
use openchamber_native_transport::attachment::AttachmentUpload;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::prompt::{Delivery, SendObservation};
use openchamber_native_transport::queue_frozen_send::{
    QueueFrozenDispatchStep, QueueFrozenSend, QueueFrozenSendOwner, QueueFrozenSource,
    QueueLocalFrozenItem, QueueLocalSendInput, QueuePreparedFile,
};
use openchamber_native_transport::queue_manifest::QueueLocalCapture;
use serde::{Deserialize, Serialize};

use crate::attachment_catalog::{self, StagingDescriptor, StagingPhase};
use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::composer_document::{self, ComposerCapture};
use crate::connection::ConnectionState;
use crate::draft_companion;
use crate::mutation_journal::JournalError;
use crate::network::NetworkTask;
use crate::queue_capture::{
    QueueNativeSource, QueueObjectId, QueueObjectKind, QueueOperationId, sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    QueueAction, QueueConfiguration, QueueDerivedFileState, QueueNativeFileDescriptor,
    QueueOperation, QueueOwner, QueueSendPhase, QueueSource, QueueStorage, QueueTransferItem,
    SendSourceCapture,
};
use crate::queue_take::QueueServer;
use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;

const INPUT_MAGIC: &[u8; 8] = b"OCLSND01";
const INPUT_LIMIT: usize = 50 * 1024 * 1024;
const HEADER_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) enum LocalSendCheck {
    #[default]
    Unknown,
    Working,
    Missing,
    Admitted,
    OutOfOrder,
    PreparationUnavailable,
    Failed,
}

struct LocalSendTask {
    runtime: RuntimeId,
    session: SessionId,
    stopped: bool,
    dispatched: bool,
    _task: Task<()>,
}

struct CapturedFiles {
    owner: DraftKey,
    files: Vec<AttachmentId>,
}

#[derive(Default)]
pub(crate) struct LocalSendState {
    tasks: HashMap<QueueOperationId, LocalSendTask>,
    checks: HashMap<QueueOperationId, LocalSendCheck>,
    // Retain expanded bytes after a refused/short put for an explicit same-window
    // retry. A restart with an incomplete input object fails closed.
    pending_inputs: HashMap<QueueOperationId, Vec<u8>>,
    captured_files: HashMap<QueueOperationId, CapturedFiles>,
}

impl LocalSendState {
    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.tasks.values().any(|task| &task.runtime == runtime)
    }
    pub(crate) fn any_busy(&self) -> bool {
        !self.tasks.is_empty()
    }
    pub(crate) fn busy(&self, id: &QueueOperationId) -> bool {
        self.tasks.contains_key(id)
    }
    pub(crate) fn check(&self, id: &QueueOperationId) -> LocalSendCheck {
        self.checks.get(id).copied().unwrap_or_default()
    }
    pub(crate) fn references_native(&self, id: &AttachmentId) -> bool {
        self.captured_files
            .values()
            .any(|capture| capture.files.contains(id))
    }
    pub(crate) fn has_native_root_holds(&self) -> bool {
        self.captured_files
            .values()
            .any(|capture| !capture.files.is_empty())
    }

    /// Hydration changes presentation only. It starts no read, write or retry,
    /// and never cancels an original runtime's task on a projection reset.
    pub(crate) fn restore(&mut self, storage: &QueueStorage) {
        self.captured_files
            .retain(|id, _| self.tasks.contains_key(id));
        self.checks.retain(|id, _| {
            self.tasks.contains_key(id)
                || storage.operation(id).is_some()
                || storage.local_send_retirement(id).is_some()
        });
        for operation in storage.operations() {
            if let QueueAction::Send { phase, .. } = operation.action()
                && !self.busy(operation.id())
            {
                self.checks.insert(
                    operation.id().clone(),
                    if matches!(phase, QueueSendPhase::Admitted { .. }) {
                        LocalSendCheck::Admitted
                    } else {
                        LocalSendCheck::Unknown
                    },
                );
            }
        }
        for retirement in storage.local_send_retirements() {
            if !self.busy(&retirement.operation) {
                self.checks.insert(
                    retirement.operation.clone(),
                    if retirement.deleted {
                        LocalSendCheck::Unknown
                    } else {
                        LocalSendCheck::Admitted
                    },
                );
            }
        }
    }

    /// Cancel only confirmed deleted owners. Their ledger terminal state must be
    /// installed first; named pre-admission roots return to the retirement owner.
    pub(crate) fn forget_deleted_sessions(
        &mut self,
        runtime: &RuntimeId,
        sessions: &[SessionId],
        operations: &[QueueOperationId],
    ) -> Vec<AttachmentId> {
        let mut ids = operations.to_vec();
        ids.extend(
            self.tasks
                .iter()
                .filter(|(_, task)| &task.runtime == runtime && sessions.contains(&task.session))
                .map(|(id, _)| id.clone()),
        );
        ids.extend(self.captured_files.iter().filter(|(_, capture)| capture.owner.runtime() == runtime
            && matches!(capture.owner.target(), DraftTarget::Session(session) if sessions.contains(session)))
            .map(|(id, _)| id.clone()));
        ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        ids.dedup();
        let mut released = Vec::new();
        for id in ids {
            let other_task = self
                .tasks
                .get(&id)
                .is_some_and(|task| &task.runtime != runtime || !sessions.contains(&task.session));
            if !other_task {
                self.tasks.remove(&id);
                self.checks.remove(&id);
                self.pending_inputs.remove(&id);
            }
            if self.captured_files.get(&id).is_some_and(|capture| capture.owner.runtime() == runtime
                && matches!(capture.owner.target(), DraftTarget::Session(session) if sessions.contains(session))) {
                released.extend(self.captured_files.remove(&id).expect("the deleted capture was checked").files);
            }
        }
        released
    }

    fn release_capture(
        &mut self,
        id: &QueueOperationId,
        runtime: &RuntimeId,
        session: &SessionId,
    ) -> Vec<AttachmentId> {
        if self.captured_files.get(id).is_some_and(|capture| {
            capture.owner.runtime() == runtime
                && capture.owner.target() == &DraftTarget::Session(session.clone())
        }) {
            self.captured_files
                .remove(id)
                .expect("the exact capture was checked")
                .files
        } else {
            Vec::new()
        }
    }

    /// Stop during local preparation or supersession wins over dispatch. Once
    /// HTTP has left, the parent must wait for this chain before interrupting.
    pub(crate) fn stop_before_dispatch(
        &mut self,
        runtime: &RuntimeId,
        session: &SessionId,
    ) -> bool {
        let mut stopped = false;
        for task in self
            .tasks
            .values_mut()
            .filter(|task| &task.runtime == runtime && &task.session == session)
        {
            if !task.dispatched {
                task.stopped = true;
                stopped = true;
            }
        }
        stopped
    }

    pub(crate) fn dispatch_in_flight(&self, runtime: &RuntimeId, session: &SessionId) -> bool {
        self.tasks
            .values()
            .any(|task| &task.runtime == runtime && &task.session == session && task.dispatched)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activation {
    Initial,
    Check,
    Retry,
}

#[derive(Debug)]
enum Failure {
    Capture,
    Preparation,
    File,
}

impl From<QueueCaptureFailure> for Failure {
    fn from(_: QueueCaptureFailure) -> Self {
        Self::Capture
    }
}
impl From<JournalError> for Failure {
    fn from(_: JournalError) -> Self {
        Self::Capture
    }
}
impl From<openchamber_native_transport::queue_manifest::QueueManifestError> for Failure {
    fn from(_: openchamber_native_transport::queue_manifest::QueueManifestError) -> Self {
        Self::Capture
    }
}
impl From<crate::composer_delivery::ComposerDeliveryError> for Failure {
    fn from(error: crate::composer_delivery::ComposerDeliveryError) -> Self {
        match error {
            crate::composer_delivery::ComposerDeliveryError::Capture(_) => Self::Capture,
            crate::composer_delivery::ComposerDeliveryError::Preparation => Self::Preparation,
            crate::composer_delivery::ComposerDeliveryError::File => Self::File,
        }
    }
}

struct InitialSend {
    capture: ComposerCapture,
    configuration: QueuedSendConfig,
    message: MessageId,
    delivery: Option<Delivery>,
    context_ids: Vec<MessageId>,
    skill_ids: Vec<MessageId>,
    files: Vec<QueueNativeFileDescriptor>,
}

impl Workspace {
    pub(crate) fn payload_local_send_blocks_session(
        &self,
        runtime: &RuntimeId,
        session: &SessionId,
    ) -> bool {
        self.queue.local_sends.tasks.iter().any(|(id, task)| &task.runtime == runtime && &task.session == session
            && !self.queue_storage.operation(id).is_some_and(|operation| operation.owner().runtime() == runtime.as_str()
                && operation.owner().session() == session.as_str()
                && matches!(operation.action(), QueueAction::Send { phase: QueueSendPhase::Deleted, .. }))
            && !self.queue_storage.local_send_retirement(id).is_some_and(|job| job.deleted
                && job.owner.runtime() == runtime.as_str() && job.owner.session() == session.as_str()))
            || self.queue_storage.operations().iter().any(|operation| {
                operation.owner().runtime() == runtime.as_str() && operation.owner().session() == session.as_str()
                    && matches!(operation.action(), QueueAction::Send { phase, .. } if !phase.terminal())
            })
    }

    /// Capture every mutable choice before spawning the first asynchronous step.
    /// Returning true means this actor owns the activation, including failures.
    pub(crate) fn begin_local_send(
        &mut self,
        delivery: Option<Delivery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.selected.clone() else {
            return false;
        };
        let key = self.mutation_key(&session);
        if !self.persistence_ready()
            || self.close_in_progress()
            || !self.prompt_allowed(&key)
            || !self.send_attempts.can_send(&key)
            || self.creations.get(&key).is_some()
            || self.payload_local_send_blocks_session(&key.runtime, &session)
            || self.payload_take_blocks_session(&key.runtime, &session)
            || self.payload_prefix_blocks_session(&key.runtime, &session)
            || self.queue_preview_pending(&session)
        {
            return false;
        }
        let Some((_, selection)) = self.send_selection() else {
            return false;
        };
        let Some(model) = selection.model else {
            return false;
        };
        let configuration = QueuedSendConfig {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: model.variant.clone(),
            agent: selection.agent_name,
        };
        let capture = match self.capture_composer_submission(cx) {
            Ok(capture) => capture,
            Err(_) => {
                self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                return true;
            }
        };
        let owner = &capture.composer.native.owner;
        if owner.runtime() != &key.runtime
            || owner.target() != &DraftTarget::Session(session.clone())
            || self.attachment_storage.has_pending(owner)
            || self
                .attachment_preparations
                .iter()
                .any(|hold| &hold.owner == owner)
        {
            return false;
        }
        let Some(record) = self.sessions.get(&session) else {
            return false;
        };
        let Some(directory) = record.directory.clone() else {
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
        let Ok(server) = QueueServer::new(
            key.runtime.clone(),
            client,
            connection.network.handle(),
            session.clone(),
            &directory,
        ) else {
            return false;
        };
        let files = match native_files(self, &capture.composer.native) {
            Ok(files) => files,
            Err(_) => {
                self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                return true;
            }
        };
        let requests = self.requests_for_send(&key);
        let message = self.message_ids.next().0;
        let Ok(id) = QueueOperationId::new(self.message_ids.next().0.as_str()) else {
            return false;
        };
        let context_ids = (0..native_context_count(&capture.composer.native))
            .map(|_| self.message_ids.next().0)
            .collect();
        // At most one unresolved-skill instruction per processed source or literal.
        // Unused IDs never acquire journal ownership or admission authority.
        let skill_ids = (0..crate::mutation_journal::MAX_ENTRIES)
            .map(|_| self.message_ids.next().0)
            .collect();
        let effort = self.capture_user_effort(&model);
        self.effort_preferences
            .retain_user_send(&key.runtime, &message, effort);
        self.spawn_local_send(
            id,
            server,
            Activation::Initial,
            Some(requests),
            Some(InitialSend {
                capture,
                configuration,
                message,
                delivery,
                context_ids,
                skill_ids,
                files,
            }),
            window,
            cx,
        );
        true
    }

    pub(crate) fn check_local_send(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_local_send(id, Activation::Check, window, cx);
    }
    pub(crate) fn retry_local_send(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.queue_storage.local_send_retirement(id).is_some()
            || self.queue_storage.operation(id).is_some_and(|operation|
                matches!(operation.action(), QueueAction::Send { phase, .. } if phase.terminal()))
        { self.retry_local_send_retirement(id, window, cx); }
        else { self.activate_local_send(id, Activation::Retry, window, cx); }
    }

    /// Resumes only journaled local retirement. It needs no connected server and
    /// cannot observe, prepare or dispatch a terminal message again.
    pub(crate) fn retry_local_send_retirement(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() || self.queue.local_sends.busy(id)
        {
            return;
        }
        let owner = self.queue_storage.local_send_retirement(id).map(|retirement| &retirement.owner)
            .or_else(|| self.queue_storage.operation(id).filter(|operation|
                matches!(operation.action(), QueueAction::Send { phase, .. } if phase.terminal())).map(QueueOperation::owner));
        let Some(owner) = owner else {
            return;
        };
        if owner.runtime() != self.runtime.as_str() && !self.queue_storage.local_send_deleted(id) {
            return;
        }
        let runtime = RuntimeId::from(owner.runtime());
        let task_runtime = runtime.clone();
        let session = SessionId::from(owner.session());
        let task_id = id.clone();
        let id = id.clone();
        self.queue
            .local_sends
            .checks
            .insert(id.clone(), LocalSendCheck::Working);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = retire(&this, cx, &id).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.queue.local_sends.tasks.remove(&id);
                // A departure held back while this transfer ran is noted now.
                cx.defer_in(window, |this, window, cx| {
                    this.reconcile_enqueue_listings(window, cx);
                });
                if this.queue_storage.local_send_retirement(&id).is_some() {
                    let check = if result.is_err() {
                        LocalSendCheck::Failed
                    } else if this.queue_storage.local_send_deleted(&id) {
                        LocalSendCheck::Unknown
                    } else {
                        LocalSendCheck::Admitted
                    };
                    this.queue.local_sends.checks.insert(id.clone(), check);
                } else {
                    this.queue.local_sends.checks.remove(&id);
                }
                if result.is_err() && this.runtime == task_runtime && !this.close_in_progress() {
                    this.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
                }
                cx.notify();
            });
        });
        self.queue.local_sends.tasks.insert(
            task_id,
            LocalSendTask {
                runtime,
                session,
                stopped: false,
                dispatched: false,
                _task: task,
            },
        );
        cx.notify();
    }

    fn activate_local_send(
        &mut self,
        id: &QueueOperationId,
        activation: Activation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() || self.queue.local_sends.busy(id)
        {
            return;
        }
        let Some(operation) = self.queue_storage.operation(id) else {
            return;
        };
        if operation.owner().runtime() != self.runtime.as_str()
            || !matches!(operation.action(), QueueAction::Send { .. })
            || self.queue_storage.local_send_deleted(id)
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
        let key = SessionKey {
            runtime: server.runtime.clone(),
            session: server.scope.session().clone(),
        };
        if activation == Activation::Retry
            && (!self.prompt_allowed(&key)
                || !self.send_attempts.can_send(&key)
                || self.payload_take_blocks_session(&key.runtime, &key.session)
                || self.payload_prefix_blocks_session(&key.runtime, &key.session)
                || self.payload_enqueue_blocks_session(&key.runtime, &key.session))
        {
            return;
        }
        let requests = (activation == Activation::Retry).then(|| self.requests_for_send(&key));
        if let QueueAction::Send { message, .. } = operation.action() {
            self.effort_preferences
                .forget_user_send(&server.runtime, &MessageId::from(message.clone()));
        }
        self.spawn_local_send(id.clone(), server, activation, requests, None, window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_local_send(
        &mut self,
        id: QueueOperationId,
        server: QueueServer,
        activation: Activation,
        requests: Option<Vec<RequestKey>>,
        initial: Option<InitialSend>,
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
                let configuration = QueueConfiguration::capture(&initial.configuration);
                let message = initial.message.clone();
                let delivery = initial.delivery;
                let contexts = initial
                    .context_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                let files = initial.files.clone();
                self.register_composer_submission(
                    id.clone(),
                    initial.capture.clone(),
                    initial.skill_ids.clone(),
                    move |capture| {
                        let mut operation = QueueOperation::local_send_with_edit(
                            operation_id.clone(),
                            queue_owner.clone(),
                            configuration.clone(),
                            message.clone(),
                            capture.composer.clone(),
                            delivery,
                            contexts.clone(),
                            files.clone(),
                            capture.edit.clone(),
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
        if let Some(initial) = &initial {
            let files = initial
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
                .collect();
            self.queue.local_sends.captured_files.insert(
                id.clone(),
                CapturedFiles {
                    owner: initial.capture.composer.native.owner.clone(),
                    files,
                },
            );
        }
        self.queue
            .local_sends
            .checks
            .insert(id.clone(), LocalSendCheck::Working);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = run(&this, cx, &id, &server, activation, requests, initial).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if result.is_err() {
                    this.settle_failed_composer_submission(&id);
                }
                let roots = this.queue.local_sends.release_capture(
                    &id,
                    &server.runtime,
                    server.scope.session(),
                );
                if this.queue_storage.local_send_deleted(&id) {
                    this.retire_released_native_roots(roots, window, cx);
                    this.resume_native_root_close(window, cx);
                    return;
                }
                let own_task = this.queue.local_sends.tasks.get(&id).is_some_and(|task| {
                    task.runtime == server.runtime && &task.session == server.scope.session()
                });
                if !own_task {
                    this.retire_released_native_roots(roots, window, cx);
                    this.resume_native_root_close(window, cx);
                    return;
                }
                this.queue.local_sends.tasks.remove(&id);
                // A departure held back while this transfer ran is noted now.
                cx.defer_in(window, |this, window, cx| {
                    this.reconcile_enqueue_listings(window, cx);
                });
                this.retire_released_native_roots(roots, window, cx);
                let check = match &result {
                    Ok(check) => *check,
                    Err(Failure::Preparation | Failure::File) => {
                        LocalSendCheck::PreparationUnavailable
                    }
                    Err(Failure::Capture) => LocalSendCheck::Failed,
                };
                if this.queue_storage.operation(&id).is_some()
                    || this.queue_storage.local_send_retirement(&id).is_some()
                {
                    this.queue.local_sends.checks.insert(id.clone(), check);
                } else {
                    this.queue.local_sends.checks.remove(&id);
                    this.queue.local_sends.pending_inputs.remove(&id);
                }
                if let Some(message) = &initial_message {
                    this.effort_preferences
                        .forget_user_send(&server.runtime, message);
                }
                if result.is_err() && this.runtime == server.runtime && !this.close_in_progress() {
                    this.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
                }
                cx.notify();
                this.resume_native_root_close(window, cx);
            });
        });
        self.queue.local_sends.tasks.insert(
            task_id,
            LocalSendTask {
                runtime,
                session,
                stopped: false,
                dispatched: false,
                _task: task,
            },
        );
        cx.notify();
    }
}

fn native_context_count(source: &QueueNativeSource) -> usize {
    source
        .draft
        .content
        .context
        .iter()
        .map(|context| {
            usize::from(context.instructions.is_some())
                + usize::from(!context.text.trim().is_empty())
        })
        .sum()
}

fn native_files(
    workspace: &Workspace,
    source: &QueueNativeSource,
) -> Result<Vec<QueueNativeFileDescriptor>, QueueCaptureFailure> {
    source
        .draft
        .content
        .files
        .iter()
        .map(|file| {
            let held = workspace
                .attachments
                .get(&file.id)
                .ok_or(QueueCaptureFailure::Invalid)?;
            if !matches!(file.content, DraftFileContent::Prepared)
                || file.byte_len != Some(held.blob.byte_len())
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            Ok(QueueNativeFileDescriptor {
                id: file.id.to_string(),
                byte_len: held.blob.byte_len(),
                sha256: *held.blob.sha256(),
            })
        })
        .collect()
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

fn owns(server: &QueueServer, operation: &QueueOperation) -> bool {
    operation.owner().runtime() == server.runtime.as_str()
        && operation.owner().session() == server.scope.session().as_str()
        && operation.owner().directory() == server.scope.directory()
}

fn local_allowed(workspace: &Workspace, id: &QueueOperationId, server: &QueueServer) -> bool {
    !workspace.close_in_progress()
        && workspace.persistence_ready()
        && workspace.composer_submission_dispatch_allowed(id)
        && workspace
            .queue
            .local_sends
            .tasks
            .get(id)
            .is_some_and(|task| {
                !task.stopped
                    && task.runtime == server.runtime
                    && &task.session == server.scope.session()
            })
        && workspace
            .queue_storage
            .operation(id)
            .is_some_and(|operation| {
                owns(server, operation)
                    && !matches!(
                        operation.action(),
                        QueueAction::Send {
                            phase: QueueSendPhase::Deleted,
                            ..
                        }
                    )
            })
}

fn dispatch_allowed(
    workspace: &Workspace,
    id: &QueueOperationId,
    server: &QueueServer,
    requests: &[RequestKey],
) -> bool {
    let key = SessionKey {
        runtime: server.runtime.clone(),
        session: server.scope.session().clone(),
    };
    local_allowed(workspace, id, server)
        && workspace.send_attempts.can_send(&key)
        && workspace.creations.get(&key).is_none()
        && (workspace.runtime != server.runtime || workspace.prompt_allowed(&key))
        && (requests.is_empty() || workspace.runtime == server.runtime)
        && workspace.requests_for_send(&key).is_empty()
        && !workspace.payload_take_blocks_session(&key.runtime, &key.session)
        && !workspace.payload_prefix_blocks_session(&key.runtime, &key.session)
        && !workspace.payload_enqueue_blocks_session(&key.runtime, &key.session)
        && !workspace.queue_storage.operations().iter().any(|other| {
            other.id() != id
                && other.owner().runtime() == server.runtime.as_str()
                && other.owner().session() == server.scope.session().as_str()
                && (workspace.queue.enqueues.busy(other.id())
                    || matches!(other.action(), QueueAction::Send { phase, .. }
                if !phase.terminal()))
        })
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

async fn read_source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<SendSourceCapture, QueueCaptureFailure> {
    let retained = operation(this, cx, id)?;
    let QueueAction::Send { source, .. } = retained.action() else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let source = match source {
        // The storage owner supplies this read-only accessor. It returns its
        // sealed typed intent, including descriptors absent from public action.
        QueueSource::Inline(_) => retained.local_send_capture()?,
        QueueSource::Companion(object) => {
            let bytes = queue_payload_io::read(this, cx, object).await?;
            cx.background_executor()
                .spawn(async move {
                    serde_json::from_slice(&bytes).map_err(|_| QueueCaptureFailure::Invalid)
                })
                .await?
        }
    };
    validate_source(&retained, &source)?;
    Ok(source)
}

fn validate_source(
    operation: &QueueOperation,
    source: &SendSourceCapture,
) -> Result<(), QueueCaptureFailure> {
    let QueueAction::Send {
        message,
        delivery,
        context_ids,
        ..
    } = operation.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    if source.version != 1
        || source.composer.native.owner != operation.owner().draft_key()?
        || source.composer.references != source.intent.companions
        || source.composer.references != operation.dependencies()
        || source.intent.edit.as_ref() != operation.composer_edit()
        || source.intent.directory != operation.owner().directory()
        || &source.intent.id != message
        || source.intent.delivery != *delivery
        || !context_ids.starts_with(&source.intent.native_context_ids)
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldFile {
    id: String,
    byte_len: u64,
    sha256: [u8; 32],
    // The native draft's safe upload name is independent of a companion's
    // model-facing name, which may be an absolute server path.
    #[serde(default)]
    native_filename: Option<String>,
    target: Option<HeldTarget>,
}

impl HeldFile {
    fn new(descriptor: BlobDescriptor, source: &QueueNativeSource) -> Self {
        let native_filename = source
            .draft
            .content
            .files
            .iter()
            .find(|file| file.id.as_str() == descriptor.id().as_str())
            .and_then(|file| file.name.clone());
        Self {
            id: descriptor.id().as_str().to_owned(),
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
            native_filename,
            target: None,
        }
    }
    fn descriptor(&self) -> Result<BlobDescriptor, QueueCaptureFailure> {
        let id = BlobId::parse(&self.id).map_err(|_| QueueCaptureFailure::Invalid)?;
        Ok(BlobDescriptor::new(id, self.byte_len, self.sha256))
    }
    fn upload_filename(&self) -> String {
        self.native_filename
            .clone()
            .unwrap_or_else(|| format!("local-{}.bin", self.id))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldTarget {
    runtime: String,
    directory: String,
    temporary_directory: String,
    server_path: String,
    uri: String,
    filename: String,
}

impl HeldTarget {
    fn capture(target: &StagingDescriptor) -> Self {
        Self {
            runtime: target.runtime.to_string(),
            directory: target.directory.clone(),
            temporary_directory: target.temporary_directory.clone(),
            server_path: target.server_path.clone(),
            uri: target.uri.clone(),
            filename: target.filename.clone(),
        }
    }
    fn restore(&self, phase: StagingPhase) -> StagingDescriptor {
        StagingDescriptor {
            runtime: RuntimeId::from(self.runtime.clone()),
            directory: self.directory.clone(),
            temporary_directory: self.temporary_directory.clone(),
            server_path: self.server_path.clone(),
            uri: self.uri.clone(),
            filename: self.filename.clone(),
            phase,
        }
    }
    fn validate(&self, runtime: &str, directory: &str) -> Result<(), QueueCaptureFailure> {
        if self.runtime != runtime
            || self.directory != directory
            || self.filename.is_empty()
            || self.uri != attachment_catalog::file_uri(&self.server_path)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        QueuePreparedFile::new_local(0, 0, [0; 32], &self.uri)
            .map_err(|_| QueueCaptureFailure::Invalid)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputSource {
    bytes: usize,
    digest: [u8; 32],
    original: Option<QueueTransferItem>,
    files: Vec<HeldFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputHeader {
    version: u32,
    runtime: String,
    endpoint: String,
    directory: String,
    session: String,
    message: String,
    configuration: QueueConfiguration,
    delivery: Option<Delivery>,
    generation: u64,
    revision: u64,
    source_digest: [u8; 32],
    staged: bool,
    sources: Vec<InputSource>,
}

struct PreparedInputs {
    header: InputHeader,
    captures: Vec<QueueLocalCapture>,
}

impl PreparedInputs {
    fn encode(&self) -> Result<Vec<u8>, QueueCaptureFailure> {
        if self.captures.len() != self.header.sources.len() {
            return Err(QueueCaptureFailure::Invalid);
        }
        let header = serde_json::to_vec(&self.header).map_err(|_| QueueCaptureFailure::Invalid)?;
        let length = self.captures.iter().try_fold(
            12usize
                .checked_add(header.len())
                .ok_or(QueueCaptureFailure::Invalid)?,
            |length, capture| {
                length
                    .checked_add(capture.bytes().len())
                    .ok_or(QueueCaptureFailure::Invalid)
            },
        )?;
        if header.len() > HEADER_LIMIT || length > INPUT_LIMIT {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(INPUT_MAGIC);
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&header);
        for (capture, saved) in self.captures.iter().zip(&self.header.sources) {
            if capture.bytes().len() != saved.bytes || capture.sha256() != saved.digest {
                return Err(QueueCaptureFailure::Invalid);
            }
            bytes.extend_from_slice(capture.bytes());
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, QueueCaptureFailure> {
        if bytes.len() < 12 || bytes.len() > INPUT_LIMIT || &bytes[..8] != INPUT_MAGIC {
            return Err(QueueCaptureFailure::Invalid);
        }
        let header_len =
            u32::from_be_bytes(bytes[8..12].try_into().expect("fixed header slice")) as usize;
        if header_len > HEADER_LIMIT {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut offset = 12usize
            .checked_add(header_len)
            .ok_or(QueueCaptureFailure::Invalid)?;
        let header: InputHeader =
            serde_json::from_slice(bytes.get(12..offset).ok_or(QueueCaptureFailure::Invalid)?)
                .map_err(|_| QueueCaptureFailure::Invalid)?;
        if header.version != 1 || header.sources.is_empty() || header.sources.len() > 21 {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut captures = Vec::with_capacity(header.sources.len());
        for source in &header.sources {
            let end = offset
                .checked_add(source.bytes)
                .ok_or(QueueCaptureFailure::Invalid)?;
            let saved = bytes.get(offset..end).ok_or(QueueCaptureFailure::Invalid)?;
            if sha256(saved) != source.digest {
                return Err(QueueCaptureFailure::Invalid);
            }
            let capture = QueueLocalCapture::from_bytes(saved.to_vec(), &header.directory, sha256)?;
            validate_files(&capture, &source.files)?;
            captures.push(capture);
            offset = end;
        }
        if offset != bytes.len() {
            return Err(QueueCaptureFailure::Invalid);
        }
        Ok(Self { header, captures })
    }

    fn validate(
        &self,
        operation: &QueueOperation,
        source: &SendSourceCapture,
        server: &QueueServer,
    ) -> Result<(), QueueCaptureFailure> {
        validate_source(operation, source)?;
        let QueueAction::Send {
            message,
            delivery,
            context_ids,
            ..
        } = operation.action()
        else {
            return Err(QueueCaptureFailure::Invalid);
        };
        let source_bytes = serde_json::to_vec(source).map_err(|_| QueueCaptureFailure::Invalid)?;
        let ids: Vec<_> = self
            .captures
            .iter()
            .flat_map(|capture| capture.context_ids().iter().map(ToString::to_string))
            .collect();
        if !owns(server, operation)
            || self.header.runtime != operation.owner().runtime()
            || self.header.endpoint != server.client.endpoint().as_str()
            || self.header.directory != operation.owner().directory()
            || self.header.session != operation.owner().session()
            || &self.header.message != message
            || self.header.delivery != *delivery
            || self.header.configuration.restore() != operation.configuration().restore()
            || self.header.source_digest != sha256(&source_bytes)
            || &ids != context_ids
            || self.captures[0].item().send_config != operation.configuration().restore()
            || self.header.sources[0].original.is_some()
            || self.captures.len() != self.header.sources.len()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut held: HashMap<&str, BlobDescriptor> = HashMap::new();
        for (capture, input) in self.captures.iter().zip(&self.header.sources) {
            validate_files(capture, &input.files)?;
            for file in &input.files {
                let descriptor = file.descriptor()?;
                let native_filename = source
                    .composer
                    .native
                    .draft
                    .content
                    .files
                    .iter()
                    .find(|native| native.id.as_str() == file.id)
                    .and_then(|native| native.name.as_deref())
                    .or_else(|| {
                        operation
                            .composer_preparation()
                            .and_then(|plan| {
                                plan.files
                                    .iter()
                                    .find(|derived| derived.local_attachment_id == file.id)
                            })
                            .map(|derived| derived.name.as_str())
                    });
                if file.native_filename.as_deref() != native_filename {
                    return Err(QueueCaptureFailure::Invalid);
                }
                if let Some(filename) = &file.native_filename {
                    AttachmentUpload::new(filename, Vec::new())
                        .map_err(|_| QueueCaptureFailure::Invalid)?;
                    if file
                        .target
                        .as_ref()
                        .is_some_and(|target| &target.filename != filename)
                    {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                }
                if held
                    .insert(&file.id, descriptor)
                    .is_some_and(|old| old != descriptor)
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
                if self.header.staged && file.target.is_none() {
                    return Err(QueueCaptureFailure::Invalid);
                }
                if let Some(target) = &file.target {
                    target.validate(&self.header.runtime, &self.header.directory)?;
                }
            }
        }
        crate::composer_delivery::validate_metadata(self.captures.iter())?;
        Ok(())
    }

    fn freeze(&self, client: &ServerClient) -> Result<QueueFrozenSend, QueueCaptureFailure> {
        if !self.header.staged {
            return Err(QueueCaptureFailure::Invalid);
        }
        let bindings = self
            .header
            .sources
            .iter()
            .map(|source| {
                source
                    .files
                    .iter()
                    .enumerate()
                    .map(|(ordinal, file)| {
                        let target = file.target.as_ref().ok_or(QueueCaptureFailure::Invalid)?;
                        target.validate(&self.header.runtime, &self.header.directory)?;
                        QueuePreparedFile::new_local(
                            ordinal,
                            file.byte_len,
                            file.sha256,
                            &target.uri,
                        )
                        .map_err(|_| QueueCaptureFailure::Invalid)
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sources: Vec<_> = self
            .captures
            .iter()
            .zip(&bindings)
            .map(|(capture, files)| {
                QueueFrozenSource::Local(QueueLocalFrozenItem { capture, files })
            })
            .collect();
        QueueFrozenSend::capture_local(
            client,
            QueueLocalSendInput {
                session: &SessionId::from(self.header.session.clone()),
                directory: &self.header.directory,
                message_id: &MessageId::from(self.header.message.clone()),
                sources: &sources,
                selected_input: 0,
                delivery: self.header.delivery,
            },
            sha256,
        )
        .map_err(|_| QueueCaptureFailure::Invalid)
    }
}

fn validate_files(
    capture: &QueueLocalCapture,
    files: &[HeldFile],
) -> Result<(), QueueCaptureFailure> {
    if capture.item().files.len() != files.len()
        || capture.item().files.iter().zip(files).any(|(file, held)| {
            file.byte_len != held.byte_len
                || file.sha256 != held.sha256
                || held.descriptor().is_err()
        })
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

fn preparing_allowed(workspace: &Workspace, id: &QueueOperationId, server: &QueueServer) -> bool {
    let key = SessionKey {
        runtime: server.runtime.clone(),
        session: server.scope.session().clone(),
    };
    !workspace.close_in_progress()
        && workspace.persistence_ready()
        && workspace.composer_submission_dispatch_allowed(id)
        && !workspace.queue_storage.local_send_deleted(id)
        && workspace
            .queue
            .local_sends
            .tasks
            .get(id)
            .is_some_and(|task| {
                !task.stopped
                    && task.runtime == server.runtime
                    && &task.session == server.scope.session()
            })
        && (workspace.runtime != server.runtime || workspace.prompt_allowed(&key))
        && workspace.send_attempts.can_send(&key)
        && workspace.creations.get(&key).is_none()
        && !workspace.payload_take_blocks_session(&key.runtime, &key.session)
        && !workspace.payload_prefix_blocks_session(&key.runtime, &key.session)
}

async fn prepare_initial(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    initial: InitialSend,
) -> Result<(), Failure> {
    let InitialSend {
        configuration,
        context_ids,
        skill_ids,
        files,
        ..
    } = initial;
    if !this
        .update_in(cx, |this, _, _| preparing_allowed(this, id, server))
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable.into());
    }
    let capture = composer_document::complete_submission(this, cx, id, false, None).await?;
    if !this
        .update_in(cx, |this, _, _| preparing_allowed(this, id, server))
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable.into());
    }
    this.update_in(cx, |this, _, _| {
        this.queue.local_sends.captured_files.remove(id);
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    // The immutable source owns the operation before any fresh file read. Every
    // derived result has a separate plan; original source facts remain sealed.
    let source = read_source(this, cx, id).await?;
    let materialized = composer_document::materialize(this, cx, &capture).await?;
    let input = crate::composer_delivery::ComposerDeliveryInput {
        inputs: materialized,
        configuration: configuration.clone(),
        native_context_ids: context_ids,
        native_files: files,
        skill_context_ids: skill_ids,
    };
    let delivery_input = crate::composer_delivery::prepare_operation(
        this,
        cx,
        id,
        server,
        capture.generation,
        capture.revision,
        false,
        input,
    )
    .await?;
    store_inputs(
        this,
        cx,
        id,
        server,
        &source,
        &capture,
        delivery_input,
        Activation::Initial,
    )
    .await?;
    finish_preparation(this, cx, id, server, Activation::Initial, Some(capture)).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn store_inputs(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    source: &SendSourceCapture,
    capture: &ComposerCapture,
    delivery_input: crate::composer_delivery::ComposerDelivery,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    let operation = operation(this, cx, id)?;
    let QueueAction::Send {
        message, delivery, ..
    } = operation.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    if delivery_input.configuration != operation.configuration().restore()
        || delivery_input
            .context_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            != match operation.action() {
                QueueAction::Send { context_ids, .. } => context_ids.clone(),
                _ => return Err(QueueCaptureFailure::Invalid),
            }
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let source_digest = *operation
        .source_sha256()
        .ok_or(QueueCaptureFailure::Invalid)?;
    let mut inputs = Vec::with_capacity(delivery_input.sources.len());
    let mut captures = Vec::with_capacity(delivery_input.sources.len());
    for source in delivery_input.sources {
        inputs.push(InputSource {
            bytes: source.capture.bytes().len(),
            digest: source.capture.sha256(),
            original: source.original,
            files: source
                .files
                .into_iter()
                .map(|file| HeldFile::new(file, &capture.composer.native))
                .collect(),
        });
        captures.push(source.capture);
    }
    let mut prepared = PreparedInputs {
        header: InputHeader {
            version: 1,
            runtime: server.runtime.to_string(),
            endpoint: server.client.endpoint().as_str().to_owned(),
            directory: server.scope.directory().to_owned(),
            session: server.scope.session().to_string(),
            message: message.to_owned(),
            configuration: operation.configuration().clone(),
            delivery: *delivery,
            generation: capture.generation,
            revision: capture.revision,
            source_digest,
            staged: false,
            sources: inputs,
        },
        captures,
    };
    this.update_in(cx, |this, _, _| {
        if !preparing_allowed(this, id, server) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let operation = this
            .queue_storage
            .operation(id)
            .ok_or(QueueCaptureFailure::Invalid)?;
        for file in prepared
            .header
            .sources
            .iter_mut()
            .flat_map(|source| &mut source.files)
        {
            let held = this
                .attachments
                .get(file.id.as_str())
                .filter(|held| {
                    file.descriptor()
                        .is_ok_and(|descriptor| descriptor == held.blob)
                })
                .ok_or(QueueCaptureFailure::Invalid)?;
            if file.native_filename.is_none() {
                file.native_filename = operation
                    .composer_preparation()
                    .and_then(|plan| {
                        plan.files
                            .iter()
                            .find(|derived| derived.local_attachment_id == file.id)
                    })
                    .map(|derived| derived.name.clone());
            }
            if let Some(target) = &held.staging {
                let target = HeldTarget::capture(target);
                target.validate(&prepared.header.runtime, &prepared.header.directory)?;
                file.target = Some(target);
            }
        }
        prepared.validate(operation, source, server)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let bytes = cx
        .background_executor()
        .spawn(async move { prepared.encode() })
        .await?;
    this.update_in(cx, |this, _, _| {
        if !local_allowed(this, id, server) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.queue
            .local_sends
            .pending_inputs
            .insert(id.clone(), bytes.clone());
        Ok(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    persist(
        this,
        cx,
        id,
        "inputs",
        QueueObjectKind::NativeCompanion,
        bytes,
        false,
        activation,
    )
    .await?;
    this.update_in(cx, |this, _, _| {
        this.queue.local_sends.pending_inputs.remove(id);
        this.release_composer_submission(id);
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    Ok(())
}

async fn resume_unsealed_preparation(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    source: &SendSourceCapture,
) -> Result<(), Failure> {
    let retained = operation(this, cx, id)?;
    let plan = retained
        .composer_preparation()
        .ok_or(QueueCaptureFailure::Invalid)?;
    if plan.inputs_object().is_some() || !plan.derived_context_ids.is_empty() {
        return Err(QueueCaptureFailure::Unavailable.into());
    }
    let capture = ComposerCapture::from_published_at(
        source.composer.clone(),
        source.intent.edit.clone(),
        plan.generation,
        plan.revision,
    )?;
    let inputs = composer_document::materialize(this, cx, &capture).await?;
    let skill_context_ids = this
        .update_in(cx, |workspace, _, _| {
            if !preparing_allowed(workspace, id, server) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            Ok(workspace
                .composer_submission_skill_ids(id)
                .unwrap_or_else(|| {
                    (0..crate::mutation_journal::MAX_ENTRIES)
                        .map(|_| workspace.message_ids.next().0)
                        .collect()
                }))
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
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
    let delivery = crate::composer_delivery::prepare_operation(
        this,
        cx,
        id,
        server,
        plan.generation,
        plan.revision,
        true,
        input,
    )
    .await?;
    store_inputs(
        this,
        cx,
        id,
        server,
        source,
        &capture,
        delivery,
        Activation::Retry,
    )
    .await?;
    Ok(())
}

async fn load_inputs(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    activation: Activation,
) -> Result<PreparedInputs, Failure> {
    let object = QueueObjectId::new(&format!("{}-inputs", id.as_str()))?;
    let verified = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .object(&object)
                .is_some_and(|object| object.verified())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    let bytes = if verified {
        queue_payload_io::read(this, cx, &object).await?
    } else {
        let bytes = this
            .update_in(cx, |this, _, _| {
                this.queue.local_sends.pending_inputs.get(id).cloned()
            })
            .ok()
            .flatten()
            .ok_or(Failure::Preparation)?;
        persist(
            this,
            cx,
            id,
            "inputs",
            QueueObjectKind::NativeCompanion,
            bytes.clone(),
            false,
            activation,
        )
        .await?;
        this.update_in(cx, |this, _, _| {
            this.queue.local_sends.pending_inputs.remove(id);
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
        bytes
    };
    Ok(cx
        .background_executor()
        .spawn(async move { PreparedInputs::decode(&bytes) })
        .await?)
}

fn same_held(workspace: &Workspace, file: &HeldFile, target: Option<&HeldTarget>) -> bool {
    workspace
        .attachments
        .get(file.id.as_str())
        .is_some_and(|attachment| {
            file.descriptor()
                .is_ok_and(|descriptor| attachment.blob == descriptor)
                && target.is_none_or(|target| {
                    attachment
                        .staging
                        .as_ref()
                        .is_some_and(|saved| HeldTarget::capture(saved) == *target)
                })
        })
}

fn set_target(
    workspace: &mut Workspace,
    file: &HeldFile,
    target: &HeldTarget,
    phase: StagingPhase,
) -> Result<(), QueueCaptureFailure> {
    let attachment = workspace
        .attachments
        .get_mut(file.id.as_str())
        .ok_or(QueueCaptureFailure::Unavailable)?;
    if attachment.blob != file.descriptor()? {
        return Err(QueueCaptureFailure::Invalid);
    }
    if attachment
        .staging
        .as_ref()
        .is_some_and(|saved| HeldTarget::capture(saved) != *target)
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let previous = attachment.staging.replace(target.restore(phase));
    if workspace
        .validate_queue_candidate(&workspace.queue_storage)
        .is_err()
    {
        workspace
            .attachments
            .get_mut(file.id.as_str())
            .expect("checked catalog descriptor")
            .staging = previous;
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

async fn stage_file(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    mut file: HeldFile,
    activation: Activation,
) -> Result<HeldFile, QueueCaptureFailure> {
    let role = format!("target-{}", file.id);
    let object = QueueObjectId::new(&format!("{}-{role}", id.as_str()))?;
    let target_object_verified = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .object(&object)
                .is_some_and(|object| object.verified())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if target_object_verified {
        let bytes = queue_payload_io::read(this, cx, &object).await?;
        let saved: HeldFile =
            serde_json::from_slice(&bytes).map_err(|_| QueueCaptureFailure::Invalid)?;
        if saved.descriptor()? != file.descriptor()?
            || saved.native_filename != file.native_filename
            || saved.target.is_none()
            || file
                .target
                .as_ref()
                .is_some_and(|target| saved.target.as_ref() != Some(target))
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        file.target = saved.target;
    }
    let descriptor = file.descriptor()?;
    let (catalog_target, read) = this
        .update_in(cx, |this, _, _| {
            if !local_allowed(this, id, server) || !same_held(this, &file, file.target.as_ref()) {
                return None;
            }
            let prepared = this.attachments.get(file.id.as_str())?;
            this.attachment_worker()
                .map(|worker| (prepared.staging.clone(), worker.read_attachment(descriptor)))
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let bytes = read
        .await
        .map_err(|_| QueueCaptureFailure::Persistence)?
        .map_err(|_| QueueCaptureFailure::Persistence)?;
    if !this
        .update_in(cx, |this, _, _| {
            local_allowed(this, id, server)
                && same_held(
                    this,
                    &file,
                    catalog_target.as_ref().map(HeldTarget::capture).as_ref(),
                )
        })
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable);
    }
    if let Some(target) = &catalog_target {
        let saved = HeldTarget::capture(target);
        saved.validate(server.runtime.as_str(), server.scope.directory())?;
        if file
            .target
            .as_ref()
            .is_some_and(|expected| expected != &saved)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        file.target = Some(saved);
    }
    if catalog_target
        .as_ref()
        .is_some_and(|target| target.phase == StagingPhase::Ready)
    {
        drop(bytes);
        let encoded = serde_json::to_vec(&file).map_err(|_| QueueCaptureFailure::Invalid)?;
        persist(
            this,
            cx,
            id,
            &role,
            QueueObjectKind::NativeCompanion,
            encoded,
            false,
            activation,
        )
        .await?;
        return Ok(file);
    }
    let client = server.client.clone();
    let directory = server.scope.directory().to_owned();
    let retained = file.target.clone();
    let filename = file.upload_filename();
    let plan = NetworkTask::spawn_on(&server.executor, async move {
        let filename = retained
            .as_ref()
            .map_or(filename.as_str(), |target| target.filename.as_str());
        let upload = AttachmentUpload::new(filename, bytes.as_slice().to_vec())?;
        drop(bytes);
        match retained {
            Some(target) => {
                client
                    .restore_attachment_plan(
                        &directory,
                        &target.temporary_directory,
                        &target.server_path,
                        upload,
                    )
                    .await
            }
            None => client.plan_attachment(&directory, upload).await,
        }
    })
    .await
    .ok_or(QueueCaptureFailure::Unavailable)?
    .map_err(|_| QueueCaptureFailure::Transport)?;
    let target = file.target.clone().unwrap_or_else(|| HeldTarget {
        runtime: server.runtime.to_string(),
        directory: server.scope.directory().to_owned(),
        temporary_directory: plan.temporary_directory().to_owned(),
        server_path: plan.server_path().to_owned(),
        uri: attachment_catalog::file_uri(plan.server_path()),
        filename: plan.filename().to_owned(),
    });
    target.validate(server.runtime.as_str(), server.scope.directory())?;
    if target.server_path != plan.server_path()
        || target.temporary_directory != plan.temporary_directory()
        || target.filename != plan.filename()
        || plan.bytes().len() as u64 != descriptor.byte_len()
        || sha256(plan.bytes()) != *descriptor.sha256()
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    this.update_in(cx, |this, _, _| {
        if !local_allowed(this, id, server) || !same_held(this, &file, file.target.as_ref()) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        set_target(this, &file, &target, StagingPhase::Writing)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    file.target = Some(target.clone());
    queue_payload_io::save(this, cx).await?;
    // An immutable per-file target record prevents a later catalog change from
    // redirecting recovery. Its descriptor and target precede the remote write.
    let encoded = serde_json::to_vec(&file).map_err(|_| QueueCaptureFailure::Invalid)?;
    persist(
        this,
        cx,
        id,
        &role,
        QueueObjectKind::NativeCompanion,
        encoded,
        false,
        activation,
    )
    .await?;
    if !this
        .update_in(cx, |this, _, _| {
            local_allowed(this, id, server) && same_held(this, &file, Some(&target))
        })
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let receipt = NetworkTask::spawn_on(&server.executor, async move { plan.write().await })
        .await
        .ok_or(QueueCaptureFailure::Unavailable)?
        .map_err(|_| QueueCaptureFailure::Transport)?;
    if receipt.server_path() != target.server_path {
        return Err(QueueCaptureFailure::Invalid);
    }
    this.update_in(cx, |this, _, _| {
        // Accepted writes retain their result even if close or Stop began during
        // the request. Neither state permits the later prompt dispatch.
        if !same_held(this, &file, Some(&target)) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        set_target(this, &file, &target, StagingPhase::Ready)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(file)
}

async fn validate_originals(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    source: &SendSourceCapture,
    inputs: &PreparedInputs,
) -> Result<(), QueueCaptureFailure> {
    let originals = draft_companion::materialize(this, cx, &source.composer).await?;
    if originals.len() + 1 != inputs.captures.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    let contexts = this
        .update_in(cx, |workspace, _, _| {
            (1..=originals.len())
                .map(|source_index| {
                    workspace
                        .queue_storage
                        .companion_context_ids(id, source_index)
                        .map(|ids| ids.into_iter().map(MessageId::from).collect::<Vec<_>>())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    for (((original, capture), input), contexts) in originals
        .iter()
        .zip(&inputs.captures[1..])
        .zip(&inputs.header.sources[1..])
        .zip(contexts)
    {
        if input
            .original
            .as_ref()
            .is_some_and(|saved| saved != &original.retained)
            || capture.item().send_config != *original.document.item().send_config()
            || capture.context_ids() != contexts
        {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    Ok(())
}

async fn seal_inputs(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    source: &SendSourceCapture,
    inputs: &PreparedInputs,
) -> Result<(), QueueCaptureFailure> {
    let object = QueueObjectId::new(&format!("{}-inputs", id.as_str()))?;
    this.update_in(cx, |workspace, _, _| {
        if !local_allowed(workspace, id, server) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        if workspace
            .queue_storage
            .operation(id)
            .is_none_or(|operation| operation.composer_preparation().is_none())
        {
            return Ok(());
        }
        workspace.change_queue_storage(|queue, workspace| {
            queue.seal_composer_inputs(
                id,
                inputs.header.generation,
                inputs.header.revision,
                object,
                &workspace.attachments,
                &workspace.attachment_preparations,
                |operation, plan, _| {
                    inputs.validate(operation, source, server).map_err(|_| {
                        JournalError::Invalid("prepared inputs differ from their captured source")
                    })?;
                    if plan.source_sha256 != inputs.header.source_digest
                        || plan.files.iter().any(|derived| {
                            !inputs
                                .header
                                .sources
                                .iter()
                                .flat_map(|source| &source.files)
                                .any(|file| {
                                    file.id == derived.local_attachment_id
                                        && file.byte_len == derived.byte_len
                                        && file.sha256 == derived.sha256
                                })
                        })
                    {
                        return Err(JournalError::Invalid("prepared inputs omit a derived file"));
                    }
                    Ok(())
                },
                |candidate| workspace.validate_queue_candidate(candidate),
            )
        })?;
        Ok(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await
}

async fn finish_preparation(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    activation: Activation,
    capture: Option<ComposerCapture>,
) -> Result<(), Failure> {
    let recovered =
        composer_document::ensure_submission_edit(this, cx, id, activation == Activation::Retry)
            .await?;
    let capture = capture.or(recovered);
    let source = read_source(this, cx, id).await?;
    let source_bytes = serde_json::to_vec(&source).map_err(|_| QueueCaptureFailure::Invalid)?;
    // A failed source put may be resumed only by this explicit activation, using
    // the original inline seal. This cannot recapture mutable composer fields.
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
    let input_object = QueueObjectId::new(&format!("{}-inputs", id.as_str()))?;
    let has_input_plan = this
        .update_in(cx, |workspace, _, _| {
            workspace.queue_storage.object(&input_object).is_some()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if !has_input_plan && activation == Activation::Retry {
        resume_unsealed_preparation(this, cx, id, server, &source).await?;
    }
    let mut inputs = load_inputs(this, cx, id, activation).await?;
    inputs.validate(&operation(this, cx, id)?, &source, server)?;
    validate_originals(this, cx, id, &source, &inputs).await?;
    seal_inputs(this, cx, id, server, &source, &inputs).await?;
    let prepared_object = QueueObjectId::new(&format!("{}-prepared", id.as_str()))?;
    let ready = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .object(&prepared_object)
                .is_some_and(|object| object.verified())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if ready {
        let bytes = queue_payload_io::read(this, cx, &prepared_object).await?;
        inputs = cx
            .background_executor()
            .spawn(async move { PreparedInputs::decode(&bytes) })
            .await?;
    } else {
        let frame_exists = this
            .update_in(cx, |this, _, _| {
                QueueObjectId::new(&format!("{}-send", id.as_str()))
                    .ok()
                    .is_some_and(|frame| this.queue_storage.object(&frame).is_some())
            })
            .unwrap_or(true);
        if frame_exists {
            return Err(QueueCaptureFailure::Invalid.into());
        }
        let mut staged = HashMap::<String, HeldFile>::new();
        for input in &mut inputs.header.sources {
            for file in &mut input.files {
                if let Some(previous) = staged.get(&file.id) {
                    if previous.descriptor()? != file.descriptor()?
                        || file
                            .target
                            .as_ref()
                            .is_some_and(|target| previous.target.as_ref() != Some(target))
                    {
                        return Err(QueueCaptureFailure::Invalid.into());
                    }
                    *file = previous.clone();
                } else {
                    *file = stage_file(this, cx, id, server, file.clone(), activation).await?;
                    staged.insert(file.id.clone(), file.clone());
                }
            }
        }
        inputs.header.staged = true;
        let (returned, encoded) = cx
            .background_executor()
            .spawn(async move {
                let encoded = inputs.encode()?;
                Ok::<_, QueueCaptureFailure>((inputs, encoded))
            })
            .await?;
        inputs = returned;
        persist(
            this,
            cx,
            id,
            "prepared",
            QueueObjectKind::NativeCompanion,
            encoded,
            false,
            activation,
        )
        .await?;
    }
    inputs.validate(&operation(this, cx, id)?, &source, server)?;
    let client = server.client.clone();
    let (inputs, bytes) = cx
        .background_executor()
        .spawn(async move {
            let bytes = inputs.freeze(&client)?.into_bytes();
            Ok::<_, QueueCaptureFailure>((inputs, bytes))
        })
        .await?;
    // Expanded sources and every original staging binding are verified and
    // durable before this call may allocate the first FrozenSend chunk.
    let frame = persist(
        this,
        cx,
        id,
        "send",
        QueueObjectKind::FrozenSend,
        bytes,
        false,
        activation,
    )
    .await?;
    let capture = match capture {
        Some(capture) => capture,
        None => capture_for_consumption(&source, &inputs.header)?,
    };
    this.update_in(cx, |this, window, cx| {
        if !local_allowed(this, id, server) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let mut candidate = this.queue_storage.clone();
        candidate.prepare_local_send(id, frame, |_| Ok(()))?;
        this.validate_queue_candidate(&candidate)?;
        let previous = std::mem::replace(&mut this.queue_storage, candidate);
        // The composer owner implements generation/revision consumption. It must
        // preserve later identical typing and perform no intervening I/O.
        if let Err(error) = this.consume_composer_submission(id, &capture, window, cx) {
            this.queue_storage = previous;
            return Err(error);
        }
        this.validate_queue_candidate(&this.queue_storage)?;
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(())
}

fn capture_for_consumption(
    source: &SendSourceCapture,
    header: &InputHeader,
) -> Result<ComposerCapture, QueueCaptureFailure> {
    ComposerCapture::from_published_at(
        source.composer.clone(),
        source.intent.edit.clone(),
        header.generation,
        header.revision,
    )
}

/// Ordinary Send restoration deliberately has no Take/Prefix fallback. Rebuild
/// from immutable prepared captures and compare the entire original frame,
/// including source capture metadata, bodies, targets and context order.
pub(crate) async fn restore_local_send(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    server: &QueueServer,
    id: &QueueOperationId,
) -> Result<QueueFrozenSend, QueueCaptureFailure> {
    let retained = operation(this, cx, id)?;
    let QueueAction::Send { phase, .. } = retained.action() else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let frame = match phase {
        QueueSendPhase::Prepared { frame }
        | QueueSendPhase::Unknown { frame }
        | QueueSendPhase::Admitted { frame } => frame,
        QueueSendPhase::Capturing | QueueSendPhase::Deleted => {
            return Err(QueueCaptureFailure::Invalid);
        }
    };
    if !owns(server, &retained) {
        return Err(QueueCaptureFailure::Invalid);
    }
    let source = read_source(this, cx, id).await?;
    let object = QueueObjectId::new(&format!("{}-prepared", id.as_str()))?;
    let bytes = queue_payload_io::read(this, cx, &object).await?;
    let inputs = cx
        .background_executor()
        .spawn(async move { PreparedInputs::decode(&bytes) })
        .await?;
    inputs.validate(&retained, &source, server)?;
    validate_originals(this, cx, id, &source, &inputs).await?;
    // Worker reads verify current byte authority, whole length and SHA-256.
    let mut checked = HashSet::new();
    for file in inputs
        .header
        .sources
        .iter()
        .flat_map(|source| &source.files)
    {
        if !checked.insert(&file.id) {
            continue;
        }
        let read = this
            .update_in(cx, |this, _, _| {
                let target = file.target.as_ref()?;
                let attachment = this.attachments.get(file.id.as_str())?;
                if this.close_in_progress()
                    || !same_held(this, file, Some(target))
                    || attachment.staging.as_ref()?.phase != StagingPhase::Ready
                {
                    return None;
                }
                let descriptor = file.descriptor().ok()?;
                this.attachment_worker()
                    .map(|worker| worker.read_attachment(descriptor))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let verified = read
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        drop(verified);
        if !this
            .update_in(cx, |this, _, _| {
                same_held(this, file, file.target.as_ref()) && !this.close_in_progress()
            })
            .unwrap_or(false)
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
    }
    let bytes = queue_payload_io::read(this, cx, frame).await?;
    let client = server.client.clone();
    cx.background_executor()
        .spawn(async move { restore_frame(&client, &retained, &inputs, bytes) })
        .await
}

fn restore_frame(
    client: &ServerClient,
    retained: &QueueOperation,
    inputs: &PreparedInputs,
    bytes: Vec<u8>,
) -> Result<QueueFrozenSend, QueueCaptureFailure> {
    let QueueAction::Send {
        message,
        context_ids,
        delivery,
        ..
    } = retained.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let expected = inputs.freeze(client)?;
    let frozen = QueueFrozenSend::from_bytes(
        client,
        QueueFrozenSendOwner {
            session: &SessionId::from(retained.owner().session()),
            directory: retained.owner().directory(),
            message_id: &MessageId::from(message.clone()),
            configuration: &retained.configuration().restore(),
        },
        bytes,
        expected.sha256(),
        sha256,
    )
    .map_err(|_| QueueCaptureFailure::Invalid)?;
    if frozen.bytes() != expected.bytes()
        || frozen.delivery() != *delivery
        || frozen
            .context_ids()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            != *context_ids
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(frozen)
}

fn supersession_complete(
    captured: &[RequestKey],
    current: &[RequestKey],
    same_runtime: bool,
    cancelled: bool,
) -> bool {
    cancelled && current.is_empty() && (captured.is_empty() || same_runtime)
}

fn prepare_next_supersession(
    workspace: &mut Workspace,
    id: &QueueOperationId,
    server: &QueueServer,
    request: &RequestKey,
) -> Result<Option<RequestAttempt>, QueueCaptureFailure> {
    let key = SessionKey {
        runtime: server.runtime.clone(),
        session: server.scope.session().clone(),
    };
    if !local_allowed(workspace, id, server)
        || workspace.runtime != server.runtime
        || request.scope.runtime() != &server.runtime
        || !workspace.prompt_allowed(&key)
        || !workspace.send_attempts.can_send(&key)
        || workspace.creations.get(&key).is_some()
    {
        return Err(QueueCaptureFailure::Unavailable);
    }
    // Acquire only the next request immediately before its HTTP attempt. Stop,
    // close or a host switch leaves every later captured request unlocked.
    workspace
        .prepare_supersession(std::slice::from_ref(request))
        .map(|mut attempts| attempts.pop())
        .map_err(|_| QueueCaptureFailure::Unavailable)
}

async fn supersede_requests(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    captured: &[RequestKey],
) -> Result<(), QueueCaptureFailure> {
    let key = SessionKey {
        runtime: server.runtime.clone(),
        session: server.scope.session().clone(),
    };
    let mut cancelled = true;
    for request in captured {
        let attempt = this
            .update_in(cx, |this, _, _| {
                prepare_next_supersession(this, id, server, request)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        let Some(attempt) = attempt else {
            continue;
        };
        let request_key = attempt.key().clone();
        let request = request_key.clone();
        let client = server.client.clone();
        let result = NetworkTask::spawn_on(&server.executor, async move {
            let reply = match request.id {
                RequestId::Permission(_) => crate::blocking_requests::RequestReply::Permission(
                    openchamber_native_core::permission::PermissionReply::Reject,
                ),
                RequestId::Form(_) => crate::blocking_requests::RequestReply::Dismiss,
            };
            crate::blocking_requests::respond(&client, &request, &reply).await
        })
        .await;
        let success =
            matches!(result, Some(Ok(())) | Some(Err(
            openchamber_native_transport::requests::RequestError::PermissionNotFound
                | openchamber_native_transport::requests::RequestError::FormNotFound
                | openchamber_native_transport::requests::RequestError::FormAlreadySettled)));
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
            local_allowed(this, id, server)
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

fn leading_contexts(expected: &[MessageId], admitted: &[MessageId]) -> bool {
    admitted.len() <= expected.len() && expected.starts_with(admitted)
}

/// Each step uses the transport's original captured body/receipt checks. This
/// foreground loop is also the close, Stop, policy and request-arrival boundary.
async fn dispatch(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    mut frozen: QueueFrozenSend,
    admitted: &[MessageId],
    captured: &[RequestKey],
) -> Result<(), QueueCaptureFailure> {
    let contexts = frozen.context_ids();
    if !leading_contexts(&contexts, admitted) {
        return Err(QueueCaptureFailure::Invalid);
    }
    let mut admitted = admitted.to_vec();
    let mut steps = vec![QueueFrozenDispatchStep::Model];
    if frozen.configuration().agent.is_some() {
        steps.push(QueueFrozenDispatchStep::Agent);
    }
    steps.extend((admitted.len()..contexts.len()).map(QueueFrozenDispatchStep::Context));
    steps.push(QueueFrozenDispatchStep::Prompt);
    for step in steps {
        this.update_in(cx, |this, _, _| {
            if !dispatch_allowed(this, id, server, captured) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            this.change_queue_storage(|queue, workspace| {
                queue.mark_send_unknown(id, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
            })?;
            Ok::<_, QueueCaptureFailure>(())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
        let allowed = this
            .update_in(cx, |this, _, _| {
                if !dispatch_allowed(this, id, server, captured) {
                    return false;
                }
                let Some(task) = this.queue.local_sends.tasks.get_mut(id) else {
                    return false;
                };
                task.dispatched = true;
                true
            })
            .unwrap_or(false);
        if !allowed {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let result = frozen.dispatch_step(step, &admitted).await;
            (frozen, admitted, result)
        });
        let (returned, returned_admitted, result) =
            request.await.ok_or(QueueCaptureFailure::Unavailable)?;
        frozen = returned;
        admitted = returned_admitted;
        result.map_err(|_| QueueCaptureFailure::Transport)?;
        if let QueueFrozenDispatchStep::Context(index) = step {
            admitted.push(contexts[index].clone());
        }
    }
    Ok(())
}

async fn record_admitted(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    message: &MessageId,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    this.update_in(cx, |this, _, _| {
        this.change_queue_storage(|queue, workspace| {
            queue.record_send_admitted(id, message, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    this.update_in(cx, |this, _, cx| {
        if activation == Activation::Initial {
            if let Some(effort) = this
                .effort_preferences
                .take_user_send(&server.runtime, message)
            {
                this.record_user_effort(effort, cx);
            }
        } else {
            this.effort_preferences
                .forget_user_send(&server.runtime, message);
        }
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    Ok(())
}

fn insert_resource(
    resources: &mut HashMap<AttachmentId, BlobDescriptor>,
    descriptor: BlobDescriptor,
) -> Result<(), QueueCaptureFailure> {
    let id = AttachmentId::from(descriptor.id().as_str());
    if resources
        .insert(id, descriptor)
        .is_some_and(|previous| previous != descriptor)
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

async fn retire(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<(), QueueCaptureFailure> {
    let retirement = this
        .update_in(cx, |this, _, _| {
            this.queue_storage.local_send_retirement(id).cloned()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    let retirement = match retirement {
        Some(retirement) => retirement,
        None if this
            .update_in(cx, |this, _, _| this.queue_storage.local_send_deleted(id))
            .unwrap_or(false) =>
        {
            // Deletion can interrupt before source/input publication. Its ledger
            // retains exact native, derived and companion obligations without reads.
            this.update_in(cx, |this, _, _| {
                if !this.persistence_ready() || this.close_in_progress() {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                let files = this.queue_storage.composer_file_descriptors(id)?;
                this.change_queue_storage(|queue, workspace| {
                    queue.begin_local_send_retirement(id, files, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })?;
                this.queue_storage
                    .local_send_retirement(id)
                    .cloned()
                    .ok_or(QueueCaptureFailure::Invalid)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??
        }
        None => {
            let source = read_source(this, cx, id).await?;
            let retained = operation(this, cx, id)?;
            if !matches!(
                retained.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Admitted { .. },
                    ..
                }
            ) {
                return Err(QueueCaptureFailure::Invalid);
            }
            let prepared = QueueObjectId::new(&format!("{}-prepared", id.as_str()))?;
            let bytes = queue_payload_io::read(this, cx, &prepared).await?;
            let inputs = cx
                .background_executor()
                .spawn(async move { PreparedInputs::decode(&bytes) })
                .await?;
            let mut resources = HashMap::new();
            for file in &source.intent.files {
                let descriptor = BlobDescriptor::new(
                    BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?,
                    file.byte_len,
                    file.sha256,
                );
                insert_resource(&mut resources, descriptor)?;
            }
            for file in inputs
                .header
                .sources
                .iter()
                .flat_map(|source| &source.files)
            {
                insert_resource(&mut resources, file.descriptor()?)?;
            }
            this.update_in(cx, |this, _, _| {
                if !this.persistence_ready() || this.close_in_progress() {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                // Removed edit files remain part of the cleanup continuation.
                // Collect them before either the consumer or its Takes can go.
                for reference in &source.composer.references {
                    let local = this
                        .queue_storage
                        .local_take(&reference.operation)
                        .ok_or(QueueCaptureFailure::Invalid)?;
                    for file in local.transfer.items.iter().flat_map(|item| &item.files) {
                        insert_resource(&mut resources, file.descriptor()?)?;
                    }
                }
                let mut files = resources
                    .values()
                    .map(|descriptor| QueueNativeFileDescriptor {
                        id: descriptor.id().as_str().to_owned(),
                        byte_len: descriptor.byte_len(),
                        sha256: *descriptor.sha256(),
                    })
                    .collect::<Vec<_>>();
                files.sort_unstable_by(|left, right| left.id.cmp(&right.id));
                this.change_queue_storage(|queue, workspace| {
                    queue.begin_local_send_retirement(id, files, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })?;
                this.queue_storage
                    .local_send_retirement(id)
                    .cloned()
                    .ok_or(QueueCaptureFailure::Invalid)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??
        }
    };
    // The continuation commits before removing its consumer. A close or crash
    // after the next save can resume without reading the retired source objects.
    queue_payload_io::save(this, cx).await?;
    let owner = retirement.owner.draft_key()?;
    let mut resources = HashMap::new();
    for file in &retirement.files {
        let descriptor = BlobDescriptor::new(
            BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?,
            file.byte_len,
            file.sha256,
        );
        insert_resource(&mut resources, descriptor)?;
    }
    let plan = this
        .update_in(cx, |this, _, _| {
            if !this.persistence_ready() || this.close_in_progress() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let operations = if this.queue_storage.operation(id).is_some() {
                std::slice::from_ref(id)
            } else {
                &[]
            };
            prepare_terminal_retirement(this, operations, &resources)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    // Consumer removal and its currently unowned attachments commit together.
    // Files still rooted by an original Take remain catalog-owned at this point.
    let mut failed = crate::queue_enqueue::release_retirement(this, cx, plan)
        .await
        .is_err();
    let writable = this
        .update_in(cx, |this, _, _| {
            this.persistence_ready() && !this.close_in_progress()
        })
        .unwrap_or(false);
    if !writable {
        return Err(QueueCaptureFailure::Persistence);
    }
    // The document owner computes unrooted versions after the consumer's removal
    // is durable. Undo, another consumer and current draft roots remain intact.
    failed |= composer_document::retire_unrooted_edits(this, cx, &owner)
        .await
        .is_err();
    let plan = this
        .update_in(cx, |this, _, _| {
            if !this.persistence_ready() || this.close_in_progress() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let removable = retirement
                .dependencies
                .iter()
                .filter(|id| {
                    this.queue_storage.operation(id).is_some()
                        && !this.queue_storage.companion_has_external_owner(id)
                })
                .cloned()
                .collect::<Vec<_>>();
            prepare_terminal_retirement(this, &removable, &resources)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    failed |= crate::queue_enqueue::release_retirement(this, cx, plan)
        .await
        .is_err();
    if failed {
        return Err(QueueCaptureFailure::Persistence);
    }
    this.update_in(cx, |this, _, _| {
        if !this.persistence_ready() || this.close_in_progress() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.change_queue_storage(|queue, workspace| {
            queue.finish_local_send_retirement(id, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
        .map_err(QueueCaptureFailure::from)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await
}

fn prepare_terminal_retirement(
    workspace: &mut Workspace,
    operations: &[QueueOperationId],
    resources: &HashMap<AttachmentId, BlobDescriptor>,
) -> Result<crate::queue_enqueue::QueueRetirement, QueueCaptureFailure> {
    let previous_queue = workspace.queue_storage.clone();
    let previous_catalog = workspace.attachments.clone();
    let previous_retired = workspace.retired_attachment_blobs.clone();
    let previous_preparations = workspace.attachment_preparations.clone();
    let result = (|| {
        let mut plan = crate::queue_enqueue::QueueRetirement {
            chunks: Vec::new(),
            attachments: Vec::new(),
        };
        let mut abandoned = HashMap::new();
        for id in operations {
            let operation = workspace
                .queue_storage
                .operation(id)
                .ok_or(JournalError::Invalid("missing terminal retirement owner"))?;
            if !matches!(
                operation.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Deleted,
                    ..
                }
            ) {
                continue;
            }
            let owner = operation.owner().draft_key()?;
            if let Some(preparation) = operation.composer_preparation() {
                for file in &preparation.files {
                    if file.state != QueueDerivedFileState::Proposed {
                        continue;
                    }
                    let hold = file.hold(&owner)?;
                    let index = workspace
                        .attachment_preparations
                        .iter()
                        .position(|saved| saved == &hold)
                        .ok_or(JournalError::Invalid(
                            "deleted derived file lost its exact preparation hold",
                        ))?;
                    workspace.attachment_preparations.remove(index);
                    abandoned.insert(hold.id, file.descriptor()?);
                }
            }
        }
        for id in operations {
            let operation = crate::queue_enqueue::prepare_retirement(workspace, id, &[])?;
            plan.chunks.extend(operation.chunks);
            plan.attachments.extend(operation.attachments);
        }
        for (resource, descriptor) in resources {
            if workspace.attachment_has_owner(resource) {
                continue;
            }
            if workspace
                .attachments
                .get(resource)
                .is_some_and(|prepared| prepared.blob != *descriptor)
            {
                return Err(JournalError::Invalid(
                    "local send retirement changed its captured attachment",
                ));
            }
            if let Some(proposed) = abandoned.get(resource) {
                if proposed != descriptor || workspace.attachments.contains_key(resource) {
                    return Err(JournalError::Invalid(
                        "deleted proposal changed its exact held descriptor",
                    ));
                }
                if !workspace.retired_attachment_blobs.contains(descriptor.id()) {
                    if workspace.retired_attachment_blobs.len()
                        >= crate::mutation_journal::MAX_ENTRIES
                    {
                        return Err(JournalError::Invalid(
                            "deleted proposal retirement capacity exceeded",
                        ));
                    }
                    workspace.retired_attachment_blobs.push(*descriptor.id());
                }
                plan.attachments.push(*descriptor.id());
                continue;
            }
            match crate::attachment_retirement::begin_retirement(
                &mut workspace.attachments,
                &mut workspace.retired_attachment_blobs,
                resource,
                *descriptor.id(),
            ) {
                crate::attachment_retirement::RetirementStart::Ready => {
                    plan.attachments.push(*descriptor.id())
                }
                // A previous independent release may already have completed.
                crate::attachment_retirement::RetirementStart::NotOwned
                    if !previous_catalog.contains_key(resource) => {}
                crate::attachment_retirement::RetirementStart::NotOwned => {
                    return Err(JournalError::Invalid(
                        "local send attachment lost retirement ownership",
                    ));
                }
                crate::attachment_retirement::RetirementStart::AtCapacity => {
                    return Err(JournalError::Invalid(
                        "local send attachment retirement capacity exceeded",
                    ));
                }
            }
        }
        plan.attachments
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        plan.attachments.dedup();
        workspace.validate_queue_candidate(&workspace.queue_storage)?;
        Ok(plan)
    })();
    if result.is_err() {
        workspace.queue_storage = previous_queue;
        workspace.attachments = previous_catalog;
        workspace.retired_attachment_blobs = previous_retired;
        workspace.attachment_preparations = previous_preparations;
    }
    result.map_err(QueueCaptureFailure::from)
}

#[derive(Debug, PartialEq, Eq)]
enum ObservedStep {
    Admitted,
    Missing,
    OutOfOrder,
    Dispatch(Vec<MessageId>),
}

fn observed_step(
    activation: Activation,
    expected: &[MessageId],
    observation: SendObservation,
) -> Result<ObservedStep, QueueCaptureFailure> {
    match observation {
        SendObservation::Pending(_) | SendObservation::Delivered(_) => Ok(ObservedStep::Admitted),
        SendObservation::OutOfOrder => Ok(ObservedStep::OutOfOrder),
        SendObservation::Missing { admitted } if !leading_contexts(expected, &admitted) => {
            Err(QueueCaptureFailure::Invalid)
        }
        SendObservation::Missing { .. } if activation == Activation::Check => {
            Ok(ObservedStep::Missing)
        }
        SendObservation::Missing { admitted } => Ok(ObservedStep::Dispatch(admitted)),
    }
}

async fn run(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    activation: Activation,
    captured: Option<Vec<RequestKey>>,
    initial: Option<InitialSend>,
) -> Result<LocalSendCheck, Failure> {
    if let Some(initial) = initial {
        prepare_initial(this, cx, id, server, initial).await?;
    }
    let retained = operation(this, cx, id)?;
    if !owns(server, &retained)
        || this
            .update_in(cx, |this, _, _| this.queue_storage.local_send_deleted(id))
            .unwrap_or(true)
    {
        return Err(QueueCaptureFailure::Invalid.into());
    }
    if matches!(
        retained.action(),
        QueueAction::Send {
            phase: QueueSendPhase::Capturing,
            ..
        }
    ) {
        if activation == Activation::Check {
            return Ok(LocalSendCheck::Unknown);
        }
        finish_preparation(this, cx, id, server, activation, None).await?;
    }
    let retained = operation(this, cx, id)?;
    let QueueAction::Send { message, phase, .. } = retained.action() else {
        return Err(QueueCaptureFailure::Invalid.into());
    };
    if matches!(phase, QueueSendPhase::Admitted { .. }) {
        if activation != Activation::Check {
            retire(this, cx, id).await?;
        }
        return Ok(LocalSendCheck::Admitted);
    }
    let message = MessageId::from(message.clone());
    let frozen = restore_local_send(this, cx, server, id).await?;
    let contexts = frozen.context_ids();
    if !this
        .update_in(cx, |this, _, _| local_allowed(this, id, server))
        .unwrap_or(false)
    {
        return Err(QueueCaptureFailure::Unavailable.into());
    }
    let observed = NetworkTask::spawn_on(&server.executor, async move {
        let observed = frozen.observe().await;
        (frozen, observed)
    })
    .await
    .ok_or(QueueCaptureFailure::Unavailable)?;
    // A failed lookup remains a failure. Only successful Missing evidence can
    // reach the explicit same-ID dispatch branch.
    let (frozen, observation) = observed;
    let admitted = match observed_step(
        activation,
        &contexts,
        observation.map_err(|_| QueueCaptureFailure::Transport)?,
    )? {
        ObservedStep::Admitted => {
            record_admitted(this, cx, id, server, &message, activation).await?;
            if activation != Activation::Check {
                retire(this, cx, id).await?;
            }
            return Ok(LocalSendCheck::Admitted);
        }
        ObservedStep::Missing => return Ok(LocalSendCheck::Missing),
        ObservedStep::OutOfOrder => return Ok(LocalSendCheck::OutOfOrder),
        ObservedStep::Dispatch(admitted) => admitted,
    };
    let captured = captured.ok_or(QueueCaptureFailure::Unavailable)?;
    supersede_requests(this, cx, id, server, &captured).await?;
    dispatch(this, cx, id, server, frozen, &admitted, &captured).await?;
    record_admitted(this, cx, id, server, &message, activation).await?;
    retire(this, cx, id).await?;
    Ok(LocalSendCheck::Admitted)
}

#[cfg(test)]
#[path = "queue_local_send_tests.rs"]
mod tests;

#[cfg(test)]
mod root_close_tests {
    use super::*;

    #[test]
    fn close_waits_for_captured_files_across_runtime_and_task_slot_changes() {
        let id = QueueOperationId::new("captured_close_owner").expect("captured identity");
        let runtime = RuntimeId::from("original-runtime");
        let session = SessionId::from("ses_original");
        let owner = DraftKey::new(
            runtime.clone(),
            "/project",
            DraftTarget::Session(session.clone()),
        )
        .expect("captured owner");
        let mut state = LocalSendState::default();
        state.tasks.insert(
            id.clone(),
            LocalSendTask {
                runtime: runtime.clone(),
                session: session.clone(),
                stopped: false,
                dispatched: true,
                _task: Task::ready(()),
            },
        );
        assert!(
            !state.has_native_root_holds(),
            "an unrelated rootless network task does not delay close"
        );
        let file = AttachmentId::from("00000000000000000000000000000001");
        let group = AttachmentId::from("original-source-group");
        state.captured_files.insert(
            id.clone(),
            CapturedFiles {
                owner,
                files: vec![file.clone(), group.clone()],
            },
        );
        state.tasks.remove(&id);
        assert!(
            state.has_native_root_holds(),
            "a missing task slot cannot discard retained capture custody"
        );
        assert!(
            state
                .release_capture(&id, &RuntimeId::from("replacement-runtime"), &session)
                .is_empty()
        );
        assert!(
            state.has_native_root_holds(),
            "the replacement runtime cannot release the original capture"
        );
        assert_eq!(
            state.release_capture(&id, &runtime, &session),
            vec![file, group]
        );
        assert!(
            !state.has_native_root_holds(),
            "only the exact capture release settles the close obligation"
        );
    }
}
