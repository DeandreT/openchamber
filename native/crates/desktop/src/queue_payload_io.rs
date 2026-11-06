//! Queue capture I/O through the existing serialized journal and private spool.

use std::sync::Arc;

use gpui_kit::{AsyncWindowContext, WeakEntity};
use openchamber_native_transport::queue_manifest::QueueManifestError;

use crate::attachment_spool::BlobId;
use crate::composer_document::{ComposerClosePermit, SubmissionClosePermit};
use crate::journal_worker::{JournalFailure, JournalReceipt};
use crate::mutation_journal::JournalError;
use crate::queue_capture::{
    QUEUE_CHUNK_BYTES, QueueCaptureBytes, QueueObjectId, QueueObjectKind, QueueOperationId, sha256,
};
use crate::queue_storage::ComposerEditRef;
use crate::queue_storage::{QueueAction, QueueObject, QueueSource, QueueStorage};
use crate::state_persistence::WriteStamp;
use crate::workspace::Workspace;

/// Classifications only. Captured content and server error bodies never escape.
#[derive(Debug)]
pub(crate) enum QueueCaptureFailure {
    Unavailable,
    Invalid,
    Persistence,
    Transport,
}

impl From<JournalError> for QueueCaptureFailure {
    fn from(_: JournalError) -> Self {
        Self::Invalid
    }
}

impl From<QueueManifestError> for QueueCaptureFailure {
    fn from(_: QueueManifestError) -> Self {
        Self::Transport
    }
}

impl Workspace {
    /// The candidate is checked with every unrelated owner still present.
    pub(crate) fn change_queue_storage<T>(
        &mut self,
        change: impl FnOnce(&mut QueueStorage, &Self) -> Result<T, JournalError>,
    ) -> Result<T, JournalError> {
        let mut queue = std::mem::take(&mut self.queue_storage);
        let result = change(&mut queue, self);
        self.queue_storage = queue;
        result
    }
}

pub(crate) async fn durable(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    barrier: Option<(WriteStamp, JournalReceipt)>,
) -> Result<(), QueueCaptureFailure> {
    let (stamp, receipt) = barrier.ok_or(QueueCaptureFailure::Unavailable)?;
    let result = receipt.await.unwrap_or(Err(JournalFailure::WorkerStopped));
    if this
        .update_in(cx, |this, _, cx| {
            this.record_persistence(stamp, &result, cx)
        })
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(QueueCaptureFailure::Persistence)
    }
}

pub(crate) async fn save(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<(), QueueCaptureFailure> {
    let barrier = this
        .update_in(cx, |this, _, cx| {
            (!this.close_in_progress())
                .then(|| this.persistence_barrier(cx))
                .flatten()
        })
        .ok()
        .flatten();
    durable(this, cx, barrier).await
}

async fn save_composer_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    edit: &ComposerEditRef,
    permit: Option<&ComposerClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    let barrier = this
        .update_in(cx, |this, _, cx| {
            this.composer_edit_save_allowed(edit, permit)
                .then(|| this.persistence_barrier(cx))
                .flatten()
        })
        .ok()
        .flatten();
    durable(this, cx, barrier).await
}

/// Only a preplanned immutable local edit may use its captured close permit.
/// This overload never publishes a Send/Prefix source or grants HTTP authority.
pub(crate) async fn persist_composer_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    edit: &ComposerEditRef,
    bytes: Vec<u8>,
    permit: Option<&ComposerClosePermit>,
    explicit_retry: bool,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    let prefix = format!("{}-", edit.operation.as_str());
    let role = edit
        .object
        .as_str()
        .strip_prefix(&prefix)
        .ok_or(QueueCaptureFailure::Invalid)?;
    if bytes.len() as u64 != edit.byte_len {
        return Err(QueueCaptureFailure::Invalid);
    }
    let activation = if explicit_retry {
        CaptureWrite::ExplicitRetry { source: false }
    } else {
        CaptureWrite::Initial { source: false }
    };
    persist_with(
        this,
        cx,
        &edit.operation,
        role,
        QueueObjectKind::NativeCompanion,
        bytes,
        activation,
        Some((edit, permit)),
        None,
    )
    .await
}

pub(crate) async fn save_submission(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    consumer: &QueueOperationId,
    permit: Option<&SubmissionClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    let barrier = this
        .update_in(cx, |workspace, _, cx| {
            workspace
                .submission_local_allowed(consumer, permit)
                .then(|| workspace.persistence_barrier(cx))
                .flatten()
        })
        .ok()
        .flatten();
    durable(this, cx, barrier).await
}

/// The pending object belongs to its original Take; every worker request also
/// binds the exact consuming operation. No current Document grants this custody.
pub(crate) async fn persist_submission_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    consumer: &QueueOperationId,
    bytes: Arc<Vec<u8>>,
    explicit_retry: bool,
    permit: Option<&SubmissionClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    let binding = this
        .update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(consumer, permit) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            workspace
                .queue_storage
                .consumer_edit_binding(consumer)
                .map_err(Into::into)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    if bytes.len() as u64 != binding.edit().byte_len {
        return Err(QueueCaptureFailure::Invalid);
    }
    save_submission(this, cx, consumer, permit).await?;
    let count = this
        .update_in(cx, |workspace, _, _| {
            workspace
                .queue_storage
                .object(&binding.edit().object)
                .map(|object| object.reference().chunks().len())
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    for ordinal in 0..count {
        let hold = this
            .update_in(cx, |workspace, _, _| {
                if !workspace.submission_local_allowed(consumer, permit)
                    || !workspace
                        .queue_storage
                        .consumer_edit_binding(consumer)
                        .as_ref()
                        .is_ok_and(|current| current == &binding)
                {
                    return Err(QueueCaptureFailure::Unavailable);
                }
                Ok(workspace
                    .queue_storage
                    .object(&binding.edit().object)
                    .and_then(|object| object.hold(ordinal)))
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)??;
        let Some(hold) = hold else {
            continue;
        };
        let retained = bytes.clone();
        let chunk = cx
            .background_executor()
            .spawn(async move {
                retained
                    .chunks(QUEUE_CHUNK_BYTES)
                    .nth(ordinal)
                    .map(<[u8]>::to_vec)
            })
            .await
            .ok_or(QueueCaptureFailure::Invalid)?;
        let pending = this
            .update_in(cx, |workspace, _, _| {
                if !workspace.submission_local_allowed(consumer, permit) {
                    return None;
                }
                let pending = workspace.attachment_worker().map(|worker| {
                    if explicit_retry {
                        worker.resume_consumer_edit_chunk(binding.clone(), hold.clone(), chunk)
                    } else {
                        worker.put_consumer_edit_chunk(binding.clone(), hold.clone(), chunk)
                    }
                });
                if pending.is_some() {
                    workspace.mark_submission_io(consumer, true);
                }
                pending
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let receipt = pending
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        this.update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(consumer, permit)
                || receipt.binding() != &binding
                || receipt.hold() != &hold
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
            workspace
                .queue_storage
                .record_consumer_edit_chunk(receipt.binding(), receipt.hold(), receipt.receipt())
                .map_err(Into::into)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
        save_submission(this, cx, consumer, permit).await?;
        this.update_in(cx, |workspace, _, _| {
            workspace.mark_submission_io(consumer, false)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    }
    let pending = this
        .update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(consumer, permit)
                || !workspace
                    .queue_storage
                    .consumer_edit_binding(consumer)
                    .as_ref()
                    .is_ok_and(|current| current == &binding)
            {
                return None;
            }
            let object = workspace.queue_storage.object(&binding.edit().object)?;
            let pending = workspace.attachment_worker().map(|worker| {
                worker.verify_consumer_edit_capture(binding.clone(), object.verification())
            });
            if pending.is_some() {
                workspace.mark_submission_io(consumer, true);
            }
            pending
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let proof = pending
        .await
        .map_err(|_| QueueCaptureFailure::Persistence)?
        .map_err(|_| QueueCaptureFailure::Persistence)?;
    this.update_in(cx, |workspace, _, _| {
        if !workspace.submission_local_allowed(consumer, permit) || proof.binding() != &binding {
            return Err(QueueCaptureFailure::Unavailable);
        }
        workspace
            .queue_storage
            .record_consumer_edit_verified(&proof)
            .map_err(Into::into)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    save_submission(this, cx, consumer, permit).await?;
    this.update_in(cx, |workspace, _, _| {
        workspace.mark_submission_io(consumer, false)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)
}

/// Close can publish only this registered operation's original local source.
pub(crate) async fn persist_submission_source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    consumer: &QueueOperationId,
    bytes: Vec<u8>,
    explicit_retry: bool,
    permit: Option<&SubmissionClosePermit>,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    let activation = if explicit_retry {
        CaptureWrite::ExplicitRetry { source: true }
    } else {
        CaptureWrite::Initial { source: true }
    };
    persist_with(
        this,
        cx,
        consumer,
        "source",
        QueueObjectKind::NativeCompanion,
        bytes,
        activation,
        None,
        Some((consumer, permit)),
    )
    .await
}

/// Exact persisted object labels survive partial puts and explicit retries.
pub(crate) async fn persist(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    operation: &QueueOperationId,
    role: &str,
    kind: QueueObjectKind,
    bytes: Vec<u8>,
    source: bool,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    persist_with(
        this,
        cx,
        operation,
        role,
        kind,
        bytes,
        CaptureWrite::Initial { source },
        None,
        None,
    )
    .await
}

/// Only an explicit activation may resume an exact unpublished chunk left by a failed put.
pub(crate) async fn persist_retry(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    operation: &QueueOperationId,
    role: &str,
    kind: QueueObjectKind,
    bytes: Vec<u8>,
    source: bool,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    persist_with(
        this,
        cx,
        operation,
        role,
        kind,
        bytes,
        CaptureWrite::ExplicitRetry { source },
        None,
        None,
    )
    .await
}

#[derive(Clone, Copy)]
enum CaptureWrite {
    Initial { source: bool },
    ExplicitRetry { source: bool },
}

#[allow(clippy::too_many_arguments)]
async fn persist_with(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    operation: &QueueOperationId,
    role: &str,
    kind: QueueObjectKind,
    bytes: Vec<u8>,
    activation: CaptureWrite,
    composer: Option<(&ComposerEditRef, Option<&ComposerClosePermit>)>,
    submission: Option<(&QueueOperationId, Option<&SubmissionClosePermit>)>,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    let source = match activation {
        CaptureWrite::Initial { source } | CaptureWrite::ExplicitRetry { source } => source,
    };
    let object_id = QueueObjectId::new(&format!("{}-{role}", operation.as_str()))?;
    let ids = this
        .update_in(cx, |this, _, _| {
            if !capture_write_allowed(this, composer, submission)
                || this.queue_storage.operation(operation).is_none()
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
            if let Some(object) = this.queue_storage.object(&object_id) {
                return object
                    .reference()
                    .chunks()
                    .iter()
                    .map(|chunk| {
                        chunk
                            .descriptor()
                            .map(|descriptor| *descriptor.id())
                            .map_err(Into::into)
                    })
                    .collect::<Result<Vec<_>, QueueCaptureFailure>>();
            }
            // Fresh local labels are unrelated to server upload names or content.
            let mut ids = Vec::new();
            for _ in 0..bytes.len().div_ceil(QUEUE_CHUNK_BYTES) {
                let digest = sha256(this.message_ids.next().0.as_str().as_bytes());
                let label = digest[..16]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let id = BlobId::parse(&label).map_err(|_| QueueCaptureFailure::Invalid)?;
                ids.push(id);
            }
            Ok(ids)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let capture = cx
        .background_executor()
        .spawn(async move { QueueCaptureBytes::new(kind, bytes, &ids) })
        .await?;
    this.update_in(cx, |this, _, _| {
        if !capture_write_allowed(this, composer, submission) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        if let Some(object) = this.queue_storage.object(&object_id) {
            if object.operation() != operation
                || object.kind() != kind
                || object.reference() != capture.reference()
            {
                return Err(QueueCaptureFailure::Invalid);
            }
        } else {
            let generation = if source {
                1
            } else {
                this.queue_storage
                    .operation(operation)
                    .and_then(|operation| operation.composer_preparation())
                    .map_or(1, |preparation| preparation.generation)
            };
            let object = QueueObject::planned(
                object_id.clone(),
                operation.clone(),
                kind,
                generation,
                capture.reference().clone(),
            )?;
            this.change_queue_storage(|queue, workspace| {
                queue
                    .admit_object(object, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                    .map_err(|(error, _)| error)
            })?;
        }
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    save_capture(this, cx, composer, submission).await?;
    let mut capture = capture;
    for ordinal in 0..capture.reference().chunks().len() {
        let hold = this
            .update_in(cx, |this, _, _| {
                this.queue_storage
                    .object(&object_id)
                    .and_then(|object| object.hold(ordinal))
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)?;
        let Some(hold) = hold else {
            continue;
        };
        let (returned, bytes) = cx
            .background_executor()
            .spawn(async move {
                let bytes = capture
                    .chunk(ordinal)
                    .expect("a held ordinal belongs to this capture")
                    .to_vec();
                (capture, bytes)
            })
            .await;
        capture = returned;
        let put = this
            .update_in(cx, |this, _, _| {
                if !capture_write_allowed(this, composer, submission) {
                    return None;
                }
                let pending = this.attachment_worker().map(|worker| match activation {
                    CaptureWrite::Initial { .. } => worker.put_queue_chunk(hold.clone(), bytes),
                    CaptureWrite::ExplicitRetry { .. } => {
                        worker.resume_queue_chunk(hold.clone(), bytes)
                    }
                });
                if pending.is_some()
                    && let Some((consumer, _)) = submission
                {
                    this.mark_submission_io(consumer, true);
                }
                pending
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let receipt = put
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        this.update_in(cx, |this, _, _| {
            if !capture_write_allowed(this, composer, submission) {
                return Err(JournalError::Invalid(
                    "capture receipt lost its local owner",
                ));
            }
            this.queue_storage.record_chunk(&hold, &receipt)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
        save_capture(this, cx, composer, submission).await?;
        if let Some((consumer, _)) = submission {
            this.update_in(cx, |workspace, _, _| {
                workspace.mark_submission_io(consumer, false)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)?;
        }
    }
    drop(capture);
    let verified = this
        .update_in(cx, |this, _, _| {
            if !capture_write_allowed(this, composer, submission) {
                return None;
            }
            let object = this.queue_storage.object(&object_id)?;
            let pending = this
                .attachment_worker()
                .map(|worker| worker.verify_queue_capture(object.verification()));
            if pending.is_some()
                && let Some((consumer, _)) = submission
            {
                this.mark_submission_io(consumer, true);
            }
            pending
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Unavailable)?;
    let proof = verified
        .await
        .map_err(|_| QueueCaptureFailure::Persistence)?
        .map_err(|_| QueueCaptureFailure::Persistence)?;
    this.update_in(cx, |this, _, _| {
        if !capture_write_allowed(this, composer, submission) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let retained = this
            .queue_storage
            .operation(operation)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let inline = matches!(
            retained.action(),
            QueueAction::Enqueue {
                source: QueueSource::Inline(_),
                ..
            } | QueueAction::Prefix {
                source: QueueSource::Inline(_),
                ..
            } | QueueAction::Send {
                source: QueueSource::Inline(_),
                ..
            }
        );
        if source && inline {
            this.change_queue_storage(|queue, workspace| {
                let retained = queue
                    .operation(operation)
                    .ok_or(JournalError::Invalid("missing publication owner"))?;
                match retained.action() {
                    QueueAction::Enqueue { .. } if retained.has_compound_source() => queue
                        .publish_enqueue_source(operation, &proof, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        }),
                    QueueAction::Prefix { .. } if retained.has_compound_source() => queue
                        .publish_prefix_source(operation, &proof, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        }),
                    QueueAction::Send { .. } => {
                        queue.publish_send_source(operation, &proof, |candidate| {
                            workspace.validate_queue_candidate(candidate)
                        })
                    }
                    QueueAction::Enqueue { .. } | QueueAction::Prefix { .. } => {
                        queue.publish_source(operation, &proof)
                    }
                    QueueAction::Take { .. } => Err(JournalError::Invalid(
                        "take has no publishable composer source",
                    )),
                }
            })?;
        } else {
            this.queue_storage.record_verified(&proof)?;
        }
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    save_capture(this, cx, composer, submission).await?;
    if let Some((consumer, _)) = submission {
        this.update_in(cx, |workspace, _, _| {
            workspace.mark_submission_io(consumer, false)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    }
    Ok(object_id)
}

fn capture_write_allowed(
    workspace: &Workspace,
    composer: Option<(&ComposerEditRef, Option<&ComposerClosePermit>)>,
    submission: Option<(&QueueOperationId, Option<&SubmissionClosePermit>)>,
) -> bool {
    if let Some((consumer, permit)) = submission {
        return workspace.submission_local_allowed(consumer, permit);
    }
    match composer {
        Some((edit, permit)) => workspace.composer_edit_save_allowed(edit, permit),
        None => !workspace.close_in_progress(),
    }
}

async fn save_capture(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    composer: Option<(&ComposerEditRef, Option<&ComposerClosePermit>)>,
    submission: Option<(&QueueOperationId, Option<&SubmissionClosePermit>)>,
) -> Result<(), QueueCaptureFailure> {
    if let Some((consumer, permit)) = submission {
        return save_submission(this, cx, consumer, permit).await;
    }
    match composer {
        Some((edit, permit)) => save_composer_edit(this, cx, edit, permit).await,
        None => save(this, cx).await,
    }
}

pub(crate) async fn read(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueObjectId,
) -> Result<Vec<u8>, QueueCaptureFailure> {
    let reference = this
        .update_in(cx, |this, _, _| {
            this.queue_storage
                .object(id)
                .filter(|object| object.verified())
                .map(|object| object.reference().clone())
        })
        .ok()
        .flatten()
        .ok_or(QueueCaptureFailure::Invalid)?;
    let mut bytes = Vec::with_capacity(reference.total_bytes() as usize);
    for chunk in reference.chunks() {
        let descriptor = chunk.descriptor()?;
        let read = this
            .update_in(cx, |this, _, _| {
                this.attachment_worker()
                    .map(|worker| worker.read_queue_chunk(descriptor))
            })
            .ok()
            .flatten()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let part = read
            .await
            .map_err(|_| QueueCaptureFailure::Persistence)?
            .map_err(|_| QueueCaptureFailure::Persistence)?;
        bytes = cx
            .background_executor()
            .spawn(async move {
                bytes.extend_from_slice(part.as_slice());
                bytes
            })
            .await;
    }
    cx.background_executor()
        .spawn(async move {
            if bytes.len() as u64 != reference.total_bytes()
                || sha256(&bytes) != *reference.sha256()
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            Ok(bytes)
        })
        .await
}
