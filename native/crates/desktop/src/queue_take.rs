//! Explicit payload take, lossless local handoff and immutable queued send.
//!
//! A window-owned task retains the original runtime through network and journal
//! waits. Hydration starts no task. Every request identity and byte label reaches
//! the journal before its first write. Only the serialized byte owner grants an
//! acknowledgement permit, after the complete local destination is durable.
//! Only explicit Retry resumes an unpublished matching prefix. Downloaded files
//! also require their original committed transfer descriptor. Admitted sends
//! retire queue chunks and unowned attachment intents in the same snapshot.

use std::collections::{HashMap, HashSet};

use gpui_kit::{AsyncWindowContext, Context, Task, WeakEntity, Window};
use openchamber_native_core::draft::{DraftKey, DraftTarget};
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedItemId;
use openchamber_native_transport::attachment::AttachmentUpload;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::prompt::{Delivery, SendObservation};
use openchamber_native_transport::queue_frozen_send::{
    QueueFrozenItem, QueueFrozenSend, QueueFrozenSendInput, QueueFrozenSendOwner, QueuePreparedFile,
};
use openchamber_native_transport::queue_manifest::{
    QueueAcknowledge, QueueAttachmentHandle, QueueIndexedPayload, QueuePayloadDocument,
    QueuePayloadFileSource, QueuePayloadFilesProof, QueuePayloadItem, QueuePayloadTake,
    QueuePayloadTakeRecord, QueuePayloadTakeState, QueuePayloadTakenItem, QueuePayloadUse,
    QueueScope,
};

use crate::attachment_catalog::{
    self, PreparationHold, PreparedAttachment, StagingDescriptor, StagingPhase,
};
use crate::attachment_io::QueueAcknowledgementPermit;
use crate::attachment_spool::{BlobDescriptor, BlobId, SpoolCommit};
use crate::connection::ConnectionState;
use crate::draft_companion::CompanionDocument;
use crate::message_queue::TakePurpose;
use crate::network::NetworkTask;
use crate::queue_capture::{
    QueueJson, QueueNativeSource, QueueObjectId, QueueObjectKind, QueueOperationId, QueueSpoolRef,
    sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    QueueAction, QueueConfiguration, QueueDestination, QueueFileBinding, QueueObject,
    QueueOperation, QueueOwner, QueueTakePhase, QueueTakePurpose, QueueTakeSendState,
    QueueTransfer, QueueTransferItem,
};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TakeCheck {
    #[default]
    Unknown,
    Working,
    NotObserved,
    LocalOwned,
    Missing,
    Admitted,
    Failed,
}

struct TakeTask {
    runtime: RuntimeId,
    _task: Task<()>,
}

#[derive(Default)]
pub(crate) struct TakeState {
    tasks: HashMap<QueueOperationId, TakeTask>,
    checks: HashMap<QueueOperationId, TakeCheck>,
}

impl TakeState {
    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.tasks.values().any(|task| &task.runtime == runtime)
    }
    pub(crate) fn any_busy(&self) -> bool {
        !self.tasks.is_empty()
    }
    pub(crate) fn busy(&self, id: &QueueOperationId) -> bool {
        self.tasks.contains_key(id)
    }
    pub(crate) fn check(&self, id: &QueueOperationId) -> TakeCheck {
        self.checks.get(id).copied().unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activation {
    Initial,
    Check,
    Retry,
}

impl Activation {
    fn write_chunk(
        self,
        worker: &crate::journal_worker::JournalWorker,
        hold: crate::queue_capture::QueueChunkHold,
        bytes: Vec<u8>,
    ) -> crate::journal_worker::BlobReceipt {
        match self {
            Self::Retry => worker.resume_queue_chunk(hold, bytes),
            Self::Initial | Self::Check => worker.put_queue_chunk(hold, bytes),
        }
    }

    fn write_attachment(
        self,
        worker: &crate::journal_worker::JournalWorker,
        hold: PreparationHold,
        descriptor: BlobDescriptor,
        bytes: Vec<u8>,
    ) -> crate::journal_worker::BlobReceipt {
        match self {
            Self::Retry => worker.resume_attachment(hold, descriptor, bytes),
            Self::Initial | Self::Check => {
                worker.put_attachment(hold.owner, hold.id, hold.generation, hold.blob, bytes)
            }
        }
    }

    async fn persist(
        self,
        this: &WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
        id: &QueueOperationId,
        role: &str,
        kind: QueueObjectKind,
        bytes: Vec<u8>,
    ) -> Result<QueueObjectId, QueueCaptureFailure> {
        match self {
            Self::Retry => {
                queue_payload_io::persist_retry(this, cx, id, role, kind, bytes, false).await
            }
            Self::Initial | Self::Check => {
                queue_payload_io::persist(this, cx, id, role, kind, bytes, false).await
            }
        }
    }
}

/// Reusable captured owner for take and prefix file preparation. A Tokio Handle
/// alone does not retain its runtime; the caller's task must expose holds_runtime.
pub(crate) struct QueueServer {
    pub(crate) runtime: RuntimeId,
    pub(crate) client: ServerClient,
    pub(crate) executor: tokio::runtime::Handle,
    pub(crate) scope: QueueScope,
}

impl QueueServer {
    pub(crate) fn new(
        runtime: RuntimeId,
        client: ServerClient,
        executor: tokio::runtime::Handle,
        session: SessionId,
        directory: &str,
    ) -> Result<Self, QueueCaptureFailure> {
        let scope = QueueScope::new(&client, session, directory)?;
        Ok(Self {
            runtime,
            client,
            executor,
            scope,
        })
    }

    fn owns(&self, owner: &QueueOwner) -> bool {
        owner.runtime() == self.runtime.as_str()
            && owner.directory() == self.scope.directory()
            && owner.session() == self.scope.session().as_str()
    }
}

/// The original index and its fully materialized capture, never a fresh preview.
pub(crate) struct MaterializedQueueItem {
    pub(crate) retained: QueueTransferItem,
    pub(crate) indexed: QueueIndexedPayload,
    pub(crate) document: QueuePayloadDocument,
    files_proof: QueuePayloadFilesProof,
}

impl MaterializedQueueItem {
    /// Run on a background executor. The proof stays bound to the complete
    /// original capture and index, including decoded legacy file bytes.
    pub(crate) fn from_materialized(
        retained: QueueTransferItem,
        indexed: QueueIndexedPayload,
        document: QueuePayloadDocument,
    ) -> Result<Self, QueueCaptureFailure> {
        let files_proof = document.files_proof(sha256)?;
        let body = indexed.body()?;
        if files_proof.capture_sha256() != &indexed.pointer().sha256()
            || files_proof.index_sha256() != &indexed.index_hash()
            || retained.index.bytes() != body.as_slice()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        Ok(Self {
            retained,
            indexed,
            document,
            files_proof,
        })
    }
}

impl Workspace {
    /// Admission and ordinary composer capture call this as well as row actions.
    pub(crate) fn payload_take_blocks_session(
        &self,
        runtime: &RuntimeId,
        session: &SessionId,
    ) -> bool {
        self.queue_storage.operations().iter().any(|operation| {
            operation.owner().runtime() == runtime.as_str()
                && operation.owner().session() == session.as_str()
                && matches!(operation.action(), QueueAction::Take { phase, purpose, .. }
                    if self.queue.takes.busy(operation.id()) || !matches!(phase, QueueTakePhase::Acknowledged { local, .. }
                        if matches!(purpose, QueueTakePurpose::Edit) || local.send == QueueTakeSendState::Admitted))
        })
    }

    pub(crate) fn begin_payload_take(
        &mut self,
        item: &QueuedItemId,
        purpose: TakePurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() {
            return;
        }
        let Some(key) = self.selected_queue_key() else {
            return;
        };
        if self.queue_preview_pending(&key.session)
            || self.queue.projection.sending(&key) == Some(item)
        {
            return;
        }
        let Some(message) = self
            .queue
            .projection
            .queue(&key)
            .iter()
            .find(|message| &message.id == item)
        else {
            return;
        };
        if !message.payload_takeable() {
            return;
        }
        let Some(configuration) = message.send_config.clone() else {
            return;
        };
        let Some(owner) = DraftKey::new(
            key.runtime.clone(),
            &key.directory,
            DraftTarget::Session(key.session.clone()),
        ) else {
            return;
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
        let executor = connection.network.handle();
        let admitted = (|| {
            let id = QueueOperationId::new(self.message_ids.next().0.as_str())?;
            let scope =
                QueueScope::new(&client, key.session.clone(), &key.directory).map_err(|_| {
                    crate::mutation_journal::JournalError::Invalid("invalid queue take owner")
                })?;
            let request_owner = scope.request(id.as_str()).map_err(|_| {
                crate::mutation_journal::JournalError::Invalid("invalid queue take identity")
            })?;
            let take =
                QueuePayloadTake::new(request_owner, vec![item.clone()], sha256).map_err(|_| {
                    crate::mutation_journal::JournalError::Invalid("invalid payload take")
                })?;
            let purpose = match purpose {
                TakePurpose::Edit => QueueTakePurpose::Edit,
                TakePurpose::Send => QueueTakePurpose::Send {
                    message: self.message_ids.next().0.to_string(),
                    delivery: self.is_streaming(&key.session).then_some(Delivery::Steer),
                },
            };
            let operation = QueueOperation::take(
                id.clone(),
                QueueOwner::new(&key.runtime, &key.directory, &key.session, &owner)?,
                QueueConfiguration::capture(&configuration),
                purpose,
                QueueJson::new(take.body().to_vec())?,
            );
            self.change_queue_storage(|queue, workspace| {
                queue
                    .admit_operation(operation, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                    .map_err(|(error, _)| error)
            })?;
            Ok::<_, crate::mutation_journal::JournalError>(id)
        })();
        match admitted {
            Ok(id) => {
                let Ok(server) =
                    QueueServer::new(key.runtime, client, executor, key.session, &key.directory)
                else {
                    return;
                };
                self.spawn_payload_take(id, server, Activation::Initial, window, cx);
            }
            Err(_) => self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx),
        }
    }

    pub(crate) fn check_payload_take(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_payload_take(id, Activation::Check, window, cx);
    }
    pub(crate) fn retry_payload_take(
        &mut self,
        id: &QueueOperationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_payload_take(id, Activation::Retry, window, cx);
    }

    fn activate_payload_take(
        &mut self,
        id: &QueueOperationId,
        activation: Activation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() || self.queue.takes.busy(id) {
            return;
        }
        let Some(operation) = self.queue_storage.operation(id) else {
            return;
        };
        if operation.owner().runtime() != self.runtime.as_str()
            || !matches!(operation.action(), QueueAction::Take { .. })
        {
            return;
        }
        if activation == Activation::Retry
            && self.payload_local_send_blocks_session(
                &self.runtime,
                &SessionId::from(operation.owner().session()),
            )
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
        self.spawn_payload_take(id.clone(), server, activation, window, cx);
    }

    fn spawn_payload_take(
        &mut self,
        id: QueueOperationId,
        server: QueueServer,
        activation: Activation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.queue.takes.busy(&id) {
            return;
        }
        let task_id = id.clone();
        let runtime = server.runtime.clone();
        self.queue
            .takes
            .checks
            .insert(id.clone(), TakeCheck::Working);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = run_take(&this, cx, &id, &server, activation).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.queue.takes.tasks.remove(&id);
                let refresh_owner = this.queue_storage.operation(&id).and_then(|operation| {
                    matches!(
                        operation.action(),
                        QueueAction::Take {
                            purpose: QueueTakePurpose::Edit,
                            phase: QueueTakePhase::LocalOwned(_)
                                | QueueTakePhase::AcknowledgementUnknown(_)
                                | QueueTakePhase::Acknowledged { .. },
                            ..
                        }
                    )
                    .then(|| operation.owner().draft_key().ok())
                    .flatten()
                });
                if this.queue_storage.operation(&id).is_some() {
                    this.queue
                        .takes
                        .checks
                        .insert(id, result.as_ref().copied().unwrap_or(TakeCheck::Failed));
                } else {
                    this.queue.takes.checks.remove(&id);
                }
                if let Some(owner) = refresh_owner
                    && this.persistence_ready()
                    && !this.close_in_progress()
                {
                    this.refresh_composer_document(&owner, window, cx);
                }
                if this.runtime == server.runtime {
                    if result.is_err() && !this.close_in_progress() {
                        this.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
                    }
                    // Presentation refresh only. No queue snapshot settles a take
                    // request, changes its identity or authorizes a retry.
                    this.read_queue_owner(
                        openchamber_native_core::message_queue::queue_owner(
                            server.scope.directory(),
                        )
                        .map(str::to_owned),
                        cx,
                    );
                }
                cx.notify();
            });
        });
        self.queue.takes.tasks.insert(
            task_id,
            TakeTask {
                runtime,
                _task: task,
            },
        );
        cx.notify();
    }
}

fn allowed(
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

fn local_blob(workspace: &mut Workspace) -> Result<BlobId, QueueCaptureFailure> {
    for _ in 0..16 {
        let digest = sha256(workspace.message_ids.next().0.as_str().as_bytes());
        let label = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let id = BlobId::parse(&label).map_err(|_| QueueCaptureFailure::Invalid)?;
        if !workspace.queue_storage.contains_blob(&id)
            && !workspace.attachments.contains_key(id.as_str())
            && !workspace
                .attachment_preparations
                .iter()
                .any(|hold| hold.blob == id)
            && !workspace.retired_attachment_blobs.contains(&id)
        {
            return Ok(id);
        }
    }
    Err(QueueCaptureFailure::Invalid)
}

/// Retains the original receipt and sealed indexes before the first download.
fn retain_transfer(
    workspace: &mut Workspace,
    id: &QueueOperationId,
    record: &QueuePayloadTakeRecord,
) -> Result<(), QueueCaptureFailure> {
    let QueuePayloadTakeState::Transferring { items } = &record.state else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let mut candidate = workspace.queue_storage.clone();
    let mut retained = Vec::new();
    for (ordinal, item) in items.iter().enumerate() {
        let object_id = QueueObjectId::new(&format!("{}-item-{ordinal}", id.as_str()))?;
        let mut chunks = Vec::new();
        for chunk in item.payload.pointer().chunks() {
            chunks.push(BlobDescriptor::new(
                local_blob(workspace)?,
                chunk.attachment.size,
                chunk.attachment.sha256,
            ));
        }
        let reference = QueueSpoolRef::new(
            item.payload.pointer().total_bytes(),
            item.payload.pointer().sha256(),
            chunks,
            QueueObjectKind::Payload,
        )?;
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
        retained.push(QueueTransferItem {
            item: item.id.to_string(),
            index: QueueJson::new(item.payload.body()?)?,
            payload: object_id,
            files: Vec::new(),
        });
    }
    candidate.record_take_transfer(
        id,
        QueueTransfer {
            receipt: QueueTransfer::capture_receipt(record)?,
            items: retained,
        },
        &workspace.attachments,
        |_| Ok(()),
    )?;
    workspace.validate_queue_candidate(&candidate)?;
    workspace.queue_storage = candidate;
    Ok(())
}

/// Downloads only original indexed chunks, using locally random labels saved
/// beforehand. Every put and final whole-stream check belongs to the worker.
async fn receive_payloads(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    transfer: &QueueTransfer,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    for item in &transfer.items {
        let index =
            QueueIndexedPayload::from_body(item.index.bytes(), server.scope.directory(), sha256)?;
        for (ordinal, chunk) in index.pointer().chunks().iter().enumerate() {
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
                    this.attachment_worker()
                        .map(|worker| activation.write_chunk(worker, hold.clone(), bytes))
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            let receipt = put
                .await
                .map_err(|_| QueueCaptureFailure::Persistence)?
                .map_err(|_| QueueCaptureFailure::Persistence)?;
            if receipt.commit != SpoolCommit::Durable {
                return Err(QueueCaptureFailure::Persistence);
            }
            this.update_in(cx, |this, _, _| {
                this.queue_storage.record_chunk(&hold, &receipt)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
            queue_payload_io::save(this, cx).await?;
        }
        let verification = this
            .update_in(cx, |this, _, _| {
                let object = this.queue_storage.object(&item.payload)?;
                this.attachment_worker()
                    .map(|worker| worker.verify_queue_capture(object.verification()))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let proof = verification
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        this.update_in(cx, |this, _, _| this.queue_storage.record_verified(&proof))
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    Ok(())
}

/// Shared by Edit, queued Send, ordinary companion capture and prefix recovery.
/// Original compact indexes are restored through body(), preserving UTF-16 seals.
pub(crate) async fn materialize_items(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    owner: &QueueOwner,
    retained: &[QueueTransferItem],
) -> Result<Vec<MaterializedQueueItem>, QueueCaptureFailure> {
    let mut items = Vec::new();
    for retained in retained {
        items.push(
            materialize_item(
                this,
                cx,
                owner.directory(),
                retained,
                QueuePayloadUse::Enqueue,
            )
            .await?,
        );
    }
    Ok(items)
}

/// Prefix composers use Composer, which permits an empty composer beside queued
/// inputs. Taken queue entries use Enqueue. Neither case rebuilds an index preview.
pub(crate) async fn materialize_item(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    directory: &str,
    retained: &QueueTransferItem,
    usage: QueuePayloadUse,
) -> Result<MaterializedQueueItem, QueueCaptureFailure> {
    let bytes = queue_payload_io::read(this, cx, &retained.payload).await?;
    let retained = retained.clone();
    let directory = directory.to_owned();
    cx.background_executor()
        .spawn(async move {
            let indexed =
                QueueIndexedPayload::from_body(retained.index.bytes(), &directory, sha256)?;
            let document = indexed.materialize(bytes, usage, sha256)?;
            MaterializedQueueItem::from_materialized(retained, indexed, document)
        })
        .await
}

fn file_binding(source: &QueuePayloadFileSource, descriptor: BlobDescriptor) -> QueueFileBinding {
    let local = descriptor.id().as_str().to_owned();
    match source {
        QueuePayloadFileSource::Manifest(blob) => QueueFileBinding::Manifest {
            blob: *blob,
            local,
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
        },
        QueuePayloadFileSource::Legacy(ordinal) => QueueFileBinding::Legacy {
            ordinal: *ordinal,
            local,
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
        },
    }
}

fn file_source(binding: &QueueFileBinding) -> QueuePayloadFileSource {
    match binding {
        QueueFileBinding::Manifest { blob, .. } => QueuePayloadFileSource::Manifest(*blob),
        QueueFileBinding::Legacy { ordinal, .. } => QueuePayloadFileSource::Legacy(*ordinal),
    }
}

/// Pure original-seal comparison. The caller must then read this exact descriptor
/// through the committed worker before binding it to any staging URI.
pub(crate) fn validate_file_bindings(
    item: &MaterializedQueueItem,
) -> Result<QueuePayloadFilesProof, QueueCaptureFailure> {
    let proof = &item.files_proof;
    if proof.capture_sha256() != &item.document.sha256()
        || proof.index_sha256() != &item.indexed.index_hash()
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    if proof.files().len() != item.retained.files.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    let mut manifest = HashSet::new();
    let mut legacy = HashSet::new();
    for file in &item.retained.files {
        let unique = match file {
            QueueFileBinding::Manifest { blob, .. } => manifest.insert(*blob),
            QueueFileBinding::Legacy { ordinal, .. } => legacy.insert(*ordinal),
        };
        if !unique {
            return Err(QueueCaptureFailure::Invalid);
        }
        let descriptor = file.descriptor()?;
        let source = file_source(file);
        if !proof.files().iter().any(|original| {
            original.source() == &source
                && original.byte_len() == descriptor.byte_len()
                && original.sha256() == descriptor.sha256()
        }) {
            return Err(QueueCaptureFailure::Invalid);
        }
    }
    Ok(proof.clone())
}

async fn receive_files(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    mut items: Vec<MaterializedQueueItem>,
    activation: Activation,
) -> Result<Vec<MaterializedQueueItem>, QueueCaptureFailure> {
    let proofs = cx
        .background_executor()
        .spawn(async move {
            let proofs = items
                .iter()
                .map(|item| item.files_proof.clone())
                .collect::<Vec<_>>();
            Ok::<_, QueueCaptureFailure>((items, proofs))
        })
        .await?;
    items = proofs.0;
    let proofs = proofs.1;
    let has_bindings = items.iter().any(|item| !item.retained.files.is_empty());
    let needs_files = proofs.iter().any(|proof| !proof.files().is_empty());
    if needs_files && !has_bindings {
        let bindings = this
            .update_in(cx, |this, _, _| {
                if this.close_in_progress() {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                let owner = this
                    .queue_storage
                    .operation(id)
                    .ok_or(QueueCaptureFailure::Unavailable)?
                    .owner()
                    .draft_key()?;
                let previous = this.attachment_preparations.clone();
                let mut files = Vec::new();
                let mut preparations = Vec::new();
                for proof in &proofs {
                    let mut bindings = Vec::new();
                    for file in proof.files() {
                        let blob = local_blob(this)?;
                        preparations.push(PreparationHold {
                            owner: owner.clone(),
                            id: AttachmentId::from(blob.as_str()),
                            generation: 1,
                            blob,
                        });
                        bindings.push(file_binding(
                            file.source(),
                            BlobDescriptor::new(blob, file.byte_len(), *file.sha256()),
                        ));
                    }
                    files.push(bindings);
                }
                this.attachment_preparations.extend(preparations);
                let result = this.change_queue_storage(|queue, workspace| {
                    queue.bind_take_files(id, files.clone(), &workspace.attachments, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                });
                if result.is_err() {
                    this.attachment_preparations = previous;
                }
                result?;
                Ok::<_, QueueCaptureFailure>(files)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        for (item, bindings) in items.iter_mut().zip(bindings) {
            item.retained.files = bindings;
        }
        queue_payload_io::save(this, cx).await?;
    }
    for item in &items {
        validate_file_bindings(item)?;
        for binding in &item.retained.files {
            let descriptor = binding.descriptor()?;
            let present = this
                .update_in(cx, |this, _, _| {
                    this.attachments
                        .get(descriptor.id().as_str())
                        .is_some_and(|prepared| prepared.blob == descriptor)
                })
                .unwrap_or(false);
            if present {
                continue;
            }
            let bytes = match (item.document.item(), file_source(binding)) {
                (QueuePayloadItem::Manifest(manifest), QueuePayloadFileSource::Manifest(blob)) => {
                    let original = manifest
                        .attachment_manifest
                        .iter()
                        .find(|file| file.attachment.blob_id == blob)
                        .ok_or(QueueCaptureFailure::Invalid)?;
                    if !allowed(this, cx, id) {
                        return Err(QueueCaptureFailure::Unavailable);
                    }
                    let handle = QueueAttachmentHandle::new(
                        server.scope.clone(),
                        original.attachment.clone(),
                    )?;
                    let request =
                        NetworkTask::spawn_on(
                            &server.executor,
                            async move { handle.download().await },
                        );
                    request
                        .await
                        .ok_or(QueueCaptureFailure::Unavailable)??
                        .into_bytes()
                }
                (QueuePayloadItem::Legacy(legacy), QueuePayloadFileSource::Legacy(ordinal)) => {
                    let file = legacy
                        .attachments
                        .get(ordinal)
                        .ok_or(QueueCaptureFailure::Invalid)?
                        .clone();
                    cx.background_executor()
                        .spawn(async move { file.bytes() })
                        .await?
                }
                _ => return Err(QueueCaptureFailure::Invalid),
            };
            let checked = cx
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
            let (hold, put) = this
                .update_in(cx, |this, _, _| {
                    if this.close_in_progress() {
                        return None;
                    }
                    let hold = this
                        .attachment_preparations
                        .iter()
                        .find(|hold| hold.blob == *descriptor.id())?
                        .clone();
                    this.attachment_worker().map(|worker| {
                        let put =
                            activation.write_attachment(worker, hold.clone(), descriptor, checked);
                        (hold, put)
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
                // Consume the accepted receipt even when a close started during I/O.
                // A failed next save leaves the old durable preparation as owner.
                let previous = this.attachments.insert(
                    hold.id.clone(),
                    PreparedAttachment {
                        blob: descriptor,
                        staging: None,
                    },
                );
                let holds = this.attachment_preparations.clone();
                this.attachment_preparations.retain(|held| held != &hold);
                if let Err(error) = this.validate_queue_candidate(&this.queue_storage) {
                    this.attachments.remove(&hold.id);
                    if let Some(previous) = previous {
                        this.attachments.insert(hold.id, previous);
                    }
                    this.attachment_preparations = holds;
                    return Err(error);
                }
                Ok(())
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
            queue_payload_io::save(this, cx).await?;
        }
    }
    Ok(items)
}

/// Shared take/prefix staging path. Every binding is checked against the original
/// materialized capture and a fresh worker read before its captured write plan.
/// Fixed targets are saved before writes and reused by explicit recovery.
pub(crate) async fn prepare_staged_files(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    operation: &QueueOperationId,
    server: &QueueServer,
    items: &[MaterializedQueueItem],
) -> Result<Vec<Vec<QueuePreparedFile>>, QueueCaptureFailure> {
    let retained = self::operation(this, cx, operation)?;
    if !server.owns(retained.owner()) {
        return Err(QueueCaptureFailure::Invalid);
    }
    let mut prepared = Vec::new();
    for item in items {
        validate_file_bindings(item)?;
        let mut files = Vec::new();
        for (ordinal, binding) in item.retained.files.iter().enumerate() {
            let descriptor = binding.descriptor()?;
            let (target, read) = this
                .update_in(cx, |this, _, _| {
                    if this.close_in_progress() {
                        return None;
                    }
                    let attachment = this.attachments.get(descriptor.id().as_str())?;
                    if attachment.blob != descriptor {
                        return None;
                    }
                    this.attachment_worker().map(|worker| {
                        (
                            attachment.staging.clone(),
                            worker.read_attachment(descriptor),
                        )
                    })
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            let bytes = read
                .await
                .map_err(|_| QueueCaptureFailure::Persistence)?
                .map_err(|_| QueueCaptureFailure::Persistence)?;
            if target.as_ref().is_some_and(|target| {
                target.runtime != server.runtime || target.directory != server.scope.directory()
            }) {
                return Err(QueueCaptureFailure::Invalid);
            }
            let uri = if let Some(target) = target
                .as_ref()
                .filter(|target| target.phase == StagingPhase::Ready)
            {
                drop(bytes);
                target.uri.clone()
            } else {
                if !allowed(this, cx, operation) {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                let client = server.client.clone();
                let directory = server.scope.directory().to_owned();
                let retained = target.clone();
                let request = NetworkTask::spawn_on(&server.executor, async move {
                    // The staging name is internal. The frozen body retains the
                    // original model-facing filename and legacy agentPath.
                    let filename = retained.as_ref().map_or_else(
                        || format!("queue-{ordinal}.bin"),
                        |target| target.filename.clone(),
                    );
                    let upload = AttachmentUpload::new(&filename, bytes.as_slice().to_vec())?;
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
                });
                let plan = request
                    .await
                    .ok_or(QueueCaptureFailure::Unavailable)?
                    .map_err(|_| QueueCaptureFailure::Transport)?;
                let mut writing = target.unwrap_or_else(|| StagingDescriptor {
                    runtime: server.runtime.clone(),
                    directory: server.scope.directory().to_owned(),
                    temporary_directory: plan.temporary_directory().to_owned(),
                    server_path: plan.server_path().to_owned(),
                    uri: attachment_catalog::file_uri(plan.server_path()),
                    filename: plan.filename().to_owned(),
                    phase: StagingPhase::Writing,
                });
                if writing.uri != attachment_catalog::file_uri(plan.server_path())
                    || writing.server_path != plan.server_path()
                    || plan.bytes().len() as u64 != descriptor.byte_len()
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
                writing.phase = StagingPhase::Writing;
                set_target(this, cx, descriptor, writing.clone())?;
                queue_payload_io::save(this, cx).await?;
                if !allowed(this, cx, operation) {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                let request =
                    NetworkTask::spawn_on(&server.executor, async move { plan.write().await });
                let receipt = request
                    .await
                    .ok_or(QueueCaptureFailure::Unavailable)?
                    .map_err(|_| QueueCaptureFailure::Transport)?;
                if receipt.server_path() != writing.server_path {
                    return Err(QueueCaptureFailure::Invalid);
                }
                writing.phase = StagingPhase::Ready;
                let uri = writing.uri.clone();
                set_target(this, cx, descriptor, writing)?;
                queue_payload_io::save(this, cx).await?;
                uri
            };
            files.push(
                QueuePreparedFile::new(
                    file_source(binding),
                    descriptor.byte_len(),
                    *descriptor.sha256(),
                    &uri,
                )
                .map_err(|_| QueueCaptureFailure::Invalid)?,
            );
        }
        prepared.push(files);
    }
    Ok(prepared)
}

fn set_target(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    descriptor: BlobDescriptor,
    target: StagingDescriptor,
) -> Result<(), QueueCaptureFailure> {
    this.update_in(cx, |this, _, _| {
        let attachment = this
            .attachments
            .get_mut(descriptor.id().as_str())
            .ok_or(QueueCaptureFailure::Unavailable)?;
        if attachment.blob != descriptor {
            return Err(QueueCaptureFailure::Invalid);
        }
        let previous = attachment.staging.replace(target);
        if this.validate_queue_candidate(&this.queue_storage).is_err() {
            this.attachments
                .get_mut(descriptor.id().as_str())
                .expect("checked attachment")
                .staging = previous;
            return Err(QueueCaptureFailure::Invalid);
        }
        Ok(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?
}

/// Pure capture shared with prefix callers. Run on the background executor, then
/// persist into a FrozenSend object before reservation, acknowledgement or dispatch.
pub(crate) fn freeze_send(
    server: &QueueServer,
    message: &MessageId,
    items: &[MaterializedQueueItem],
    files: &[Vec<QueuePreparedFile>],
    selected_input: usize,
    delivery: Option<Delivery>,
) -> Result<QueueFrozenSend, QueueCaptureFailure> {
    if items.len() != files.len() {
        return Err(QueueCaptureFailure::Invalid);
    }
    let input: Vec<_> = items
        .iter()
        .zip(files)
        .map(|(item, files)| QueueFrozenItem {
            document: &item.document,
            indexed_payload: &item.indexed,
            files,
        })
        .collect();
    QueueFrozenSend::capture(
        &server.client,
        QueueFrozenSendInput {
            session: server.scope.session(),
            directory: server.scope.directory(),
            message_id: message,
            items: &input,
            selected_input,
            delivery,
        },
        sha256,
    )
    .map_err(|_| QueueCaptureFailure::Invalid)
}

async fn prepare_destination(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    record: &QueuePayloadTakeRecord,
    activation: Activation,
) -> Result<(), QueueCaptureFailure> {
    let retained = operation(this, cx, id)?;
    let QueueAction::Take {
        purpose,
        phase: QueueTakePhase::Transferring(transfer),
        ..
    } = retained.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    receive_payloads(this, cx, id, server, transfer, activation).await?;
    let items = materialize_items(this, cx, retained.owner(), &transfer.items).await?;
    let items = receive_files(this, cx, id, server, items, activation).await?;
    let edit = matches!(purpose, QueueTakePurpose::Edit);
    let native = this
        .update_in(cx, |this, _, cx| {
            if this.close_in_progress() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let owner = retained.owner().draft_key()?;
            this.save_composer_draft(cx);
            Ok::<_, QueueCaptureFailure>(QueueNativeSource {
                draft: if edit {
                    this.drafts.get(&owner).cloned().unwrap_or_default()
                } else {
                    Default::default()
                },
                owner,
            })
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    this.update_in(cx, |this, _, _| {
        this.change_queue_storage(|queue, workspace| {
            queue.retain_take_native_source(id, &native, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let (items, companion) = cx
        .background_executor()
        .spawn(async move {
            let companion = CompanionDocument::new(
                native,
                items.iter().map(|item| item.retained.clone()).collect(),
            );
            let bytes = serde_json::to_vec(&companion).map_err(|_| QueueCaptureFailure::Invalid)?;
            Ok::<_, QueueCaptureFailure>((items, bytes))
        })
        .await?;
    // Existing composer text can legitimately change after an interrupted partial
    // put. A new destination label preserves that new capture and every older hold.
    let role = format!(
        "destination-{}",
        sha256(&companion)[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let companion = activation
        .persist(
            this,
            cx,
            id,
            &role,
            QueueObjectKind::NativeCompanion,
            companion,
        )
        .await?;
    let destination = match purpose {
        QueueTakePurpose::Edit => QueueDestination::Draft { companion },
        QueueTakePurpose::Send { message, delivery } => {
            let files = prepare_staged_files(this, cx, id, server, &items).await?;
            let message_id = MessageId::from(message.clone());
            let client = server.client.clone();
            let runtime = server.runtime.clone();
            let executor = server.executor.clone();
            let session = server.scope.session().clone();
            let directory = server.scope.directory().to_owned();
            let delivery = *delivery;
            let bytes = cx
                .background_executor()
                .spawn(async move {
                    let server = QueueServer::new(runtime, client, executor, session, &directory)?;
                    freeze_send(&server, &message_id, &items, &files, 0, delivery)
                        .map(QueueFrozenSend::into_bytes)
                })
                .await?;
            let prompt = activation
                .persist(this, cx, id, "send", QueueObjectKind::FrozenSend, bytes)
                .await?;
            QueueDestination::Send {
                companion,
                prompt,
                message: message.clone(),
            }
        }
    };
    let ack = QueueAcknowledge::new(server.scope.request(id.as_str())?, record.receipt.clone())?;
    let acknowledge = QueueJson::new(ack.body().to_vec())?;
    this.update_in(cx, |this, _, _| {
        if this.close_in_progress() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.change_queue_storage(|queue, workspace| {
            queue.prepare_take_destination(
                id,
                destination,
                acknowledge,
                &workspace.attachments,
                |candidate| workspace.validate_queue_candidate(candidate),
            )
        })?;
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    if edit {
        let owner = retained.owner().draft_key()?;
        this.update_in(cx, |this, window, cx| {
            if this.persistence_ready() && !this.close_in_progress() {
                this.refresh_composer_document(&owner, window, cx);
            }
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    }
    Ok(())
}

async fn acknowledgement_permit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<QueueAcknowledgementPermit, QueueCaptureFailure> {
    let retained = operation(this, cx, id)?;
    let local = this
        .update_in(cx, |this, _, _| this.queue_storage.local_take(id).cloned())
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let items = materialize_items(this, cx, retained.owner(), &local.transfer.items).await?;
    let proofs = cx
        .background_executor()
        .spawn(async move {
            items
                .iter()
                .map(validate_file_bindings)
                .collect::<Result<Vec<_>, _>>()
        })
        .await?;
    let permit = this
        .update_in(cx, |this, _, _| {
            this.attachment_worker()
                .map(|worker| worker.authorize_queue_acknowledgement(id.clone(), proofs))
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    permit
        .await
        .map_err(|_| QueueCaptureFailure::Persistence)?
        .map_err(|_| QueueCaptureFailure::Persistence)
}

/// Reconstructs exact frozen bytes for take/prefix explicit recovery. It never
/// re-resolves catalogs, file paths, context IDs or the mutable composer.
pub(crate) async fn restore_frozen_send(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    server: &QueueServer,
    operation: &QueueOperationId,
    prompt: &QueueObjectId,
    message: &MessageId,
) -> Result<QueueFrozenSend, QueueCaptureFailure> {
    let retained = self::operation(this, cx, operation)?;
    if !server.owns(retained.owner()) {
        return Err(QueueCaptureFailure::Invalid);
    }
    let digest = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .object(prompt)
                .filter(|object| {
                    object.operation() == operation
                        && object.kind() == QueueObjectKind::FrozenSend
                        && object.verified()
                })
                .map(|object| *object.reference().sha256())
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Invalid)?;
    let bytes = queue_payload_io::read(this, cx, prompt).await?;
    let client = server.client.clone();
    let session = server.scope.session().clone();
    let directory = server.scope.directory().to_owned();
    let message = message.clone();
    let config = retained.configuration().restore();
    let delivery = match retained.action() {
        QueueAction::Take {
            purpose:
                QueueTakePurpose::Send {
                    message: expected,
                    delivery,
                },
            ..
        } if expected.as_str() == message.as_str() => *delivery,
        QueueAction::Prefix {
            message: expected,
            request,
            ..
        } if expected.as_str() == message.as_str() => {
            let body = request.value()?;
            if body.get("delivery").is_some() {
                Some(Delivery::Steer)
            } else {
                None
            }
        }
        _ => return Err(QueueCaptureFailure::Invalid),
    };
    cx.background_executor()
        .spawn(async move {
            let frozen = QueueFrozenSend::from_bytes(
                &client,
                QueueFrozenSendOwner {
                    session: &session,
                    directory: &directory,
                    message_id: &message,
                    configuration: &config,
                },
                bytes,
                digest,
                sha256,
            )
            .map_err(|_| QueueCaptureFailure::Invalid)?;
            if frozen.delivery() != delivery {
                return Err(QueueCaptureFailure::Invalid);
            }
            Ok(frozen)
        })
        .await
}

async fn run_frozen_send(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    activation: Activation,
) -> Result<TakeCheck, QueueCaptureFailure> {
    let retained = operation(this, cx, id)?;
    let QueueAction::Take {
        phase: QueueTakePhase::Acknowledged { local, .. },
        ..
    } = retained.action()
    else {
        return Err(QueueCaptureFailure::Invalid);
    };
    if local.send == QueueTakeSendState::Admitted {
        if activation != Activation::Check {
            retire_admitted_send(this, cx, id).await?;
        }
        return Ok(TakeCheck::Admitted);
    }
    let QueueDestination::Send {
        prompt, message, ..
    } = &local.destination
    else {
        return Ok(TakeCheck::LocalOwned);
    };
    let frozen = restore_frozen_send(
        this,
        cx,
        server,
        id,
        prompt,
        &MessageId::from(message.clone()),
    )
    .await?;
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let observe = NetworkTask::spawn_on(&server.executor, async move {
        let observed = frozen.observe().await;
        (frozen, observed)
    });
    let (frozen, observed) = observe.await.ok_or(QueueCaptureFailure::Unavailable)?;
    let observed = observed.map_err(|_| QueueCaptureFailure::Transport)?;
    let admitted = match observed {
        SendObservation::Pending(_) | SendObservation::Delivered(_) => {
            record_send_admitted(this, cx, id).await?;
            if activation != Activation::Check {
                retire_admitted_send(this, cx, id).await?;
            }
            return Ok(TakeCheck::Admitted);
        }
        SendObservation::Missing { admitted } => admitted,
        SendObservation::OutOfOrder => return Err(QueueCaptureFailure::Invalid),
    };
    if activation == Activation::Check {
        return Ok(TakeCheck::Missing);
    }
    this.update_in(cx, |this, _, _| {
        this.change_queue_storage(|queue, workspace| {
            queue.record_take_send_state(id, QueueTakeSendState::Unknown, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let request =
        NetworkTask::spawn_on(
            &server.executor,
            async move { frozen.dispatch(&admitted).await },
        );
    request
        .await
        .ok_or(QueueCaptureFailure::Unavailable)?
        .map_err(|_| QueueCaptureFailure::Transport)?;
    record_send_admitted(this, cx, id).await?;
    retire_admitted_send(this, cx, id).await?;
    Ok(TakeCheck::Admitted)
}

async fn retire_admitted_send(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<(), QueueCaptureFailure> {
    let files = this
        .update_in(cx, |this, _, _| {
            this.queue_storage.local_take(id).map(|local| {
                local
                    .transfer
                    .items
                    .iter()
                    .flat_map(|item| &item.files)
                    .map(|file| {
                        file.descriptor()
                            .map(|descriptor| AttachmentId::from(descriptor.id().as_str()))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)??;
    // Both retirement namespaces commit before either release. A chunk failure
    // cannot lose the attachment IDs when the operation has already retired.
    crate::queue_enqueue::retire_with_attachments(this, cx, id, &files).await
}

async fn record_send_admitted(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<(), QueueCaptureFailure> {
    this.update_in(cx, |this, _, _| {
        this.change_queue_storage(|queue, workspace| {
            queue.record_take_send_state(id, QueueTakeSendState::Admitted, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await
}

async fn run_take(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    activation: Activation,
) -> Result<TakeCheck, QueueCaptureFailure> {
    // The original request body and purpose are durable before POST or lookup.
    queue_payload_io::save(this, cx).await?;
    let retained = operation(this, cx, id)?;
    let QueueAction::Take { request, phase, .. } = retained.action() else {
        return Err(QueueCaptureFailure::Invalid);
    };
    let take = QueuePayloadTake::from_body(
        server.scope.request(id.as_str())?,
        request.bytes().to_vec(),
        sha256,
    )?;
    let lookup = activation != Activation::Initial || !matches!(phase, QueueTakePhase::Requested);
    if !allowed(this, cx, id) {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let observed = if lookup {
        let request = NetworkTask::spawn_on(&server.executor, async move {
            let observed = take.lookup().await;
            (take, observed)
        });
        let (take, observed) = request.await.ok_or(QueueCaptureFailure::Unavailable)?;
        (take, observed?)
    } else {
        (take, None)
    };
    let (take, observed) = observed;
    let record = match observed {
        Some(record) => record,
        None => {
            if !matches!(phase, QueueTakePhase::Requested | QueueTakePhase::Unknown) {
                return Err(QueueCaptureFailure::Invalid);
            }
            if activation == Activation::Check {
                return Ok(TakeCheck::NotObserved);
            }
            this.update_in(cx, |this, _, _| this.queue_storage.mark_unknown(id))
                .map_err(|_| QueueCaptureFailure::Unavailable)??;
            queue_payload_io::save(this, cx).await?;
            if !allowed(this, cx, id) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let request =
                NetworkTask::spawn_on(&server.executor, async move { take.submit().await });
            request.await.ok_or(QueueCaptureFailure::Unavailable)??
        }
    };
    if matches!(phase, QueueTakePhase::Acknowledged { .. })
        && !matches!(record.state, QueuePayloadTakeState::Acknowledged)
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    let has_transfer = matches!(
        phase,
        QueueTakePhase::Transferring(_)
            | QueueTakePhase::LocalOwned(_)
            | QueueTakePhase::AcknowledgementUnknown(_)
            | QueueTakePhase::Acknowledged { .. }
    );
    if has_transfer {
        let original = match phase {
            QueueTakePhase::Transferring(transfer) => &transfer.receipt,
            QueueTakePhase::LocalOwned(local)
            | QueueTakePhase::AcknowledgementUnknown(local)
            | QueueTakePhase::Acknowledged { local, .. } => &local.transfer.receipt,
            _ => unreachable!(),
        };
        if original.value()?.get("receipt")
            != QueueTransfer::capture_receipt(&record)?
                .value()?
                .get("receipt")
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        if let QueuePayloadTakeState::Transferring { items } = &record.state {
            let old = match phase {
                QueueTakePhase::Transferring(transfer) => &transfer.items,
                QueueTakePhase::LocalOwned(local)
                | QueueTakePhase::AcknowledgementUnknown(local)
                | QueueTakePhase::Acknowledged { local, .. } => &local.transfer.items,
                _ => unreachable!(),
            };
            if !same_indexes(old, items)? {
                return Err(QueueCaptureFailure::Invalid);
            }
        }
    } else {
        if matches!(record.state, QueuePayloadTakeState::Acknowledged) {
            return Err(QueueCaptureFailure::Invalid);
        }
        this.update_in(cx, |this, _, _| retain_transfer(this, id, &record))
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    let retained = operation(this, cx, id)?;
    if matches!(
        retained.action(),
        QueueAction::Take {
            phase: QueueTakePhase::Transferring(_),
            ..
        }
    ) {
        // Check status can retain an authoritative receipt but performs no
        // downloads, byte writes, local publication or acknowledgements.
        if activation == Activation::Check {
            return Ok(TakeCheck::Unknown);
        }
        if !matches!(record.state, QueuePayloadTakeState::Transferring { .. }) {
            return Err(QueueCaptureFailure::Invalid);
        }
        prepare_destination(this, cx, id, server, &record, activation).await?;
    }
    let acknowledged = matches!(
        operation(this, cx, id)?.action(),
        QueueAction::Take {
            phase: QueueTakePhase::Acknowledged { .. },
            ..
        }
    );
    if !acknowledged {
        if activation == Activation::Check
            && !matches!(record.state, QueuePayloadTakeState::Acknowledged)
        {
            return Ok(TakeCheck::LocalOwned);
        }
        let permit = acknowledgement_permit(this, cx, id).await?;
        let observed = if matches!(record.state, QueuePayloadTakeState::Acknowledged) {
            QueueTransfer::capture_receipt(&record)?
        } else {
            let ack =
                QueueAcknowledge::new(server.scope.request(id.as_str())?, record.receipt.clone())?;
            if ack.body() != permit.request().bytes() {
                return Err(QueueCaptureFailure::Invalid);
            }
            this.update_in(cx, |this, _, _| this.queue_storage.mark_unknown(id))
                .map_err(|_| QueueCaptureFailure::Unavailable)??;
            queue_payload_io::save(this, cx).await?;
            if !allowed(this, cx, id) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let request =
                NetworkTask::spawn_on(&server.executor, async move { ack.submit().await });
            let record = request.await.ok_or(QueueCaptureFailure::Unavailable)??;
            QueueTransfer::capture_receipt(&QueuePayloadTakeRecord {
                receipt: record.receipt,
                state: QueuePayloadTakeState::Acknowledged,
                revision: record.revision,
            })?
        };
        this.update_in(cx, |this, _, _| {
            this.change_queue_storage(|queue, workspace| {
                queue.record_take_acknowledged(&permit, observed, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
            })
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    let purpose = operation(this, cx, id)?;
    match purpose.action() {
        QueueAction::Take {
            purpose: QueueTakePurpose::Edit,
            ..
        } => Ok(TakeCheck::LocalOwned),
        QueueAction::Take {
            purpose: QueueTakePurpose::Send { .. },
            ..
        } => run_frozen_send(this, cx, id, server, activation).await,
        _ => Err(QueueCaptureFailure::Invalid),
    }
}

fn same_indexes(
    retained: &[QueueTransferItem],
    observed: &[QueuePayloadTakenItem],
) -> Result<bool, QueueCaptureFailure> {
    if retained.len() != observed.len() {
        return Ok(false);
    }
    for (old, new) in retained.iter().zip(observed) {
        let body = new.payload.body()?;
        if old.item != new.id.as_str() || old.index.bytes() != body.as_slice() {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
#[path = "queue_take_tests.rs"]
pub(crate) mod tests;
