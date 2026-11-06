//! Window-owned submission captures and their local durable handoff.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};

use gpui_kit::{AsyncWindowContext, Context, WeakEntity, Window};
use openchamber_native_core::draft::{DraftKey, DraftTarget};
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use tokio::sync::oneshot;

use super::{ComposerCapture, ComposerDocuments, EditDocument};
use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::mutation_journal::{JournalError, MAX_ENTRIES, Owner};
use crate::queue_capture::{
    QUEUE_CHUNK_BYTES, QueueObjectId, QueueObjectKind, QueueOperationId, QueueSpoolRef, sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    CapturedComposerSource, ComposerEditRef, QueueObject, QueueOperation, QueueSource,
};
use crate::workspace::Workspace;

type BuildOperation = Box<dyn Fn(&ComposerCapture) -> Result<QueueOperation, JournalError>>;

pub(super) struct ComposerSubmission {
    capture: ComposerCapture,
    authority: Arc<CapturedComposerSource>,
    build: BuildOperation,
    edit: Option<EncodedEdit>,
    operation: Option<QueueOperation>,
    admitted: bool,
    skill_ids: Vec<MessageId>,
    active: Weak<SubmissionFlight>,
    failed: bool,
    local_complete: bool,
    pending_io: bool,
    close_epoch: Option<u64>,
    dispatch_revoked: bool,
}

#[derive(Clone)]
struct EncodedEdit {
    reference: ComposerEditRef,
    object: Option<QueueObject>,
    bytes: Arc<Vec<u8>>,
}

#[derive(Default)]
struct SubmissionFlight {
    waiters: Mutex<Vec<oneshot::Sender<()>>>,
}

impl SubmissionFlight {
    fn wait(&self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.waiters
            .lock()
            .expect("submission waiters are not poisoned")
            .push(sender);
        receiver
    }
}

impl Drop for SubmissionFlight {
    fn drop(&mut self) {
        for waiter in self
            .waiters
            .get_mut()
            .expect("submission waiters are not poisoned")
            .drain(..)
        {
            let _ = waiter.send(());
        }
    }
}

/// Only the owned foreground close barrier issues this local-only capability.
#[derive(Clone)]
pub(crate) struct SubmissionClosePermit {
    epoch: u64,
    operation: QueueOperationId,
    authority: Arc<CapturedComposerSource>,
}

impl ComposerDocuments {
    pub(super) fn forget_submission_sessions(
        &mut self,
        runtime: &RuntimeId,
        sessions: &HashSet<&SessionId>,
    ) -> Vec<AttachmentId> {
        let ids: Vec<_> = self.submissions.iter().filter(|(_, submission)| {
            let owner = &submission.capture.composer.native.owner;
            owner.runtime() == runtime && matches!(owner.target(), DraftTarget::Session(session) if sessions.contains(session))
        }).map(|(id, _)| id.clone()).collect();
        let mut roots = Vec::new();
        for id in ids {
            if let Some(submission) = self.submissions.remove(&id) {
                roots.extend(
                    submission
                        .capture
                        .composer
                        .native
                        .draft
                        .content
                        .files
                        .iter()
                        .flat_map(|file| {
                            std::iter::once(file.id.clone()).chain(file.source_document_id.clone())
                        }),
                );
            }
            self.submission_retries.remove(&id);
        }
        roots
    }
}

impl Workspace {
    pub(crate) fn register_composer_submission(
        &mut self,
        id: QueueOperationId,
        capture: ComposerCapture,
        skill_ids: Vec<MessageId>,
        build: impl Fn(&ComposerCapture) -> Result<QueueOperation, JournalError> + 'static,
    ) -> Result<(), QueueCaptureFailure> {
        if self.close_in_progress()
            || self.composer_documents.submissions.len() >= MAX_ENTRIES
            || self.composer_documents.submissions.contains_key(&id)
            || self
                .composer_documents
                .submissions
                .values()
                .any(|submission| {
                    !submission.local_complete
                        && submission.capture.composer.native.owner == capture.composer.native.owner
                })
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let roots = capture
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
        let authority = self.queue_storage.capture_composer_source(
            id.clone(),
            capture.composer.native.owner.clone(),
            capture.composer.references.clone(),
            capture.edit.clone().or_else(|| capture.planned.clone()),
            roots,
        )?;
        self.composer_documents.submissions.insert(
            id,
            ComposerSubmission {
                capture,
                authority,
                build: Box::new(build),
                edit: None,
                operation: None,
                admitted: false,
                skill_ids,
                active: Weak::new(),
                failed: false,
                local_complete: false,
                pending_io: false,
                close_epoch: None,
                dispatch_revoked: false,
            },
        );
        Ok(())
    }

    pub(crate) fn composer_submissions_pending(&self) -> bool {
        self.composer_documents
            .submissions
            .values()
            .any(|submission| !submission.local_complete)
    }

    pub(crate) fn composer_submission_failed(&self, owner: &DraftKey) -> bool {
        self.composer_documents
            .submissions
            .values()
            .any(|submission| {
                &submission.capture.composer.native.owner == owner
                    && (submission.failed
                        || (submission.pending_io && submission.active.upgrade().is_none()))
            })
    }

    pub(crate) fn composer_submission_skill_ids(
        &self,
        id: &QueueOperationId,
    ) -> Option<Vec<MessageId>> {
        self.composer_documents
            .submissions
            .get(id)
            .map(|submission| submission.skill_ids.clone())
    }

    pub(crate) fn composer_submission_dispatch_allowed(&self, id: &QueueOperationId) -> bool {
        self.composer_documents
            .submissions
            .get(id)
            .is_none_or(|submission| !submission.dispatch_revoked)
    }

    pub(crate) fn release_composer_submission(&mut self, id: &QueueOperationId) {
        if self
            .composer_documents
            .submissions
            .get(id)
            .is_some_and(|submission| submission.local_complete)
        {
            self.composer_documents.submissions.remove(id);
        }
    }

    fn abort_submission_if_current_retains(&mut self, id: &QueueOperationId) -> bool {
        let Some(submission) = self.composer_documents.submissions.get(id) else {
            return false;
        };
        if submission.admitted {
            return false;
        }
        let capture = &submission.capture;
        let owner = &capture.composer.native.owner;
        let current = self
            .drafts
            .get(owner)
            .is_some_and(|draft| draft.same_content(&capture.composer.native.draft));
        let exact = match self.composer_documents.documents.get(owner) {
            Some(document) => {
                (document.generation, document.revision) == (capture.generation, capture.revision)
                    && document.known == capture.composer.references
            }
            None => {
                capture.composer.references.is_empty()
                    && self
                        .composer_documents
                        .generations
                        .get(owner)
                        .copied()
                        .unwrap_or(1)
                        == capture.generation
                    && self
                        .composer_documents
                        .native_revisions
                        .get(owner)
                        .copied()
                        .unwrap_or(1)
                        == capture.revision
            }
        };
        if !current || !exact {
            return false;
        }
        self.composer_documents.submissions.remove(id);
        true
    }

    pub(crate) fn settle_failed_composer_submission(&mut self, id: &QueueOperationId) {
        if self.abort_submission_if_current_retains(id) {
            return;
        }
        if let Some(submission) = self
            .composer_documents
            .submissions
            .get_mut(id)
            .filter(|submission| !submission.local_complete)
        {
            submission.failed = true;
        }
    }

    pub(crate) fn mark_submission_io(&mut self, id: &QueueOperationId, pending: bool) {
        if let Some(submission) = self.composer_documents.submissions.get_mut(id) {
            submission.pending_io = pending;
        }
    }

    pub(crate) fn submission_local_allowed(
        &self,
        id: &QueueOperationId,
        permit: Option<&SubmissionClosePermit>,
    ) -> bool {
        if !self.persistence_loaded() || self.persistence_closing() {
            return false;
        }
        let Some(submission) = self.composer_documents.submissions.get(id) else {
            return false;
        };
        if self.queue_storage.local_send_deleted(id) {
            return false;
        }
        match permit {
            Some(permit) => {
                self.composer_close_in_progress()
                    && self.composer_documents.frozen == Some(permit.epoch)
                    && &permit.operation == id
                    && Arc::ptr_eq(&permit.authority, &submission.authority)
            }
            None => {
                (!self.close_in_progress() && self.composer_documents.frozen.is_none())
                    || (submission.active.upgrade().is_some()
                        && submission.close_epoch.is_some()
                        && ((self.composer_close_in_progress()
                            && submission.close_epoch == self.composer_documents.frozen)
                            || (self.composer_hydration_close_in_progress()
                                && submission.close_epoch == Some(self.composer_documents.epoch))))
            }
        }
    }

    pub(crate) fn submission_close_permits(&mut self) -> Vec<SubmissionClosePermit> {
        let Some(epoch) = self.composer_documents.frozen else {
            return Vec::new();
        };
        self.composer_documents
            .submissions
            .iter_mut()
            .filter(|(_, submission)| !submission.local_complete)
            .map(|(id, submission)| {
                submission.close_epoch = Some(epoch);
                submission.dispatch_revoked = true;
                SubmissionClosePermit {
                    epoch,
                    operation: id.clone(),
                    authority: submission.authority.clone(),
                }
            })
            .collect()
    }

    pub(crate) fn authorize_submission_hydration_close(&mut self) {
        let epoch = self.composer_documents.epoch;
        for submission in self
            .composer_documents
            .submissions
            .values_mut()
            .filter(|submission| !submission.local_complete)
        {
            submission.close_epoch = Some(epoch);
            submission.dispatch_revoked = true;
        }
    }

    /// Existing Storage Retry repairs only retained local work. It cannot dispatch.
    pub(crate) fn retry_composer_submissions(
        &mut self,
        owner: Option<&DraftKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_in_progress() {
            return;
        }
        let ids: Vec<_> = self
            .composer_documents
            .submissions
            .iter()
            .filter(|(id, submission)| {
                !submission.local_complete
                    && submission.active.upgrade().is_none()
                    && !self.composer_documents.submission_retries.contains_key(*id)
                    && owner.is_none_or(|owner| &submission.capture.composer.native.owner == owner)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let task_id = id.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                let result = complete_submission(&this, cx, &id, true, None).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.composer_documents.submission_retries.remove(&id);
                    if result.is_err() {
                        this.notify_mutation_failure(
                            "chat.nativeRecovery.storageFailed",
                            window,
                            cx,
                        );
                    }
                    this.resume_composer_close(window, cx);
                    cx.notify();
                });
            });
            self.composer_documents
                .submission_retries
                .insert(task_id, task);
        }
    }
}

/// A missing same-window capture never permits recreating an unfinished edit.
pub(crate) async fn ensure_submission_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    explicit_retry: bool,
) -> Result<Option<ComposerCapture>, QueueCaptureFailure> {
    let registered = this
        .update_in(cx, |workspace, _, _| {
            workspace.composer_documents.submissions.contains_key(id)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if registered {
        return complete_submission(this, cx, id, explicit_retry, None)
            .await
            .map(Some);
    }
    this.update_in(cx, |workspace, _, _| {
        let operation = workspace
            .queue_storage
            .operation(id)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        if operation.composer_edit().is_some_and(|edit| {
            workspace
                .queue_storage
                .object(&edit.object)
                .is_none_or(|object| !object.verified())
        }) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        Ok(None)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?
}

pub(crate) async fn drain_submissions(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    permits: Vec<SubmissionClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    for permit in permits {
        loop {
            let waiting = this
                .update_in(cx, |workspace, _, _| {
                    workspace
                        .composer_documents
                        .submissions
                        .get(&permit.operation)
                        .and_then(|submission| submission.active.upgrade())
                        .map(|flight| flight.wait())
                })
                .map_err(|_| QueueCaptureFailure::Unavailable)?;
            match waiting {
                Some(waiting) => {
                    let _ = waiting.await;
                }
                None => break,
            }
        }
        let needed = this
            .update_in(cx, |workspace, _, _| {
                workspace
                    .composer_documents
                    .submissions
                    .get(&permit.operation)
                    .is_some_and(|submission| !submission.local_complete)
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)?;
        if needed
            && let Err(error) =
                complete_submission(this, cx, &permit.operation, false, Some(&permit)).await
        {
            let retained = this
                .update_in(cx, |workspace, _, _| {
                    workspace
                        .composer_documents
                        .submissions
                        .contains_key(&permit.operation)
                })
                .map_err(|_| QueueCaptureFailure::Unavailable)?;
            if retained {
                return Err(error);
            }
        }
    }
    Ok(())
}

pub(crate) async fn complete_submission(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    explicit_retry: bool,
    permit: Option<&SubmissionClosePermit>,
) -> Result<ComposerCapture, QueueCaptureFailure> {
    let flight = this
        .update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(id, permit) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let submission = workspace
                .composer_documents
                .submissions
                .get_mut(id)
                .expect("registered submission was checked");
            if submission.active.upgrade().is_some()
                || ((submission.failed || submission.pending_io) && !explicit_retry)
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
            if explicit_retry {
                submission.failed = false;
                submission.pending_io = false;
                submission.dispatch_revoked = false;
            }
            let flight = Arc::new(SubmissionFlight::default());
            submission.active = Arc::downgrade(&flight);
            Ok(flight)
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let result = complete_local(this, cx, id, explicit_retry, permit).await;
    let _ = this.update_in(cx, |workspace, _, cx| {
        let close_takeover = permit.is_none()
            && workspace.composer_documents.frozen.is_some()
            && matches!(&result, Err(QueueCaptureFailure::Unavailable));
        if let Some(submission) = workspace.composer_documents.submissions.get_mut(id) {
            // Close invalidates the ordinary actor at the next local boundary.
            // Its close barrier then takes over without granting repair authority.
            if result.is_err() && !close_takeover {
                submission.failed = true;
            }
        }
        if result.is_err() {
            workspace.abort_submission_if_current_retains(id);
        }
        cx.notify();
    });
    drop(flight);
    result
}

async fn complete_local(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    explicit_retry: bool,
    permit: Option<&SubmissionClosePermit>,
) -> Result<ComposerCapture, QueueCaptureFailure> {
    encode_edit(this, cx, id, permit).await?;
    let capture = this
        .update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(id, permit) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let submission = workspace
                .composer_documents
                .submissions
                .get(id)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            if submission.local_complete {
                return Ok(submission.capture.clone());
            }
            let operation = match &submission.operation {
                Some(operation) => operation.clone(),
                None => (submission.build)(&submission.capture)?,
            };
            let authority = submission.authority.clone();
            let pending = submission
                .edit
                .as_ref()
                .and_then(|edit| edit.object.clone());
            let admitted = submission.admitted;
            workspace
                .composer_documents
                .submissions
                .get_mut(id)
                .expect("registered capture remains owned")
                .operation = Some(operation.clone());
            if !admitted {
                let count = workspace
                    .queue_storage
                    .required_companion_rebinding_ids(&operation)?;
                let ids = (0..count)
                    .map(|_| workspace.message_ids.next().0.to_string())
                    .collect();
                workspace.change_queue_storage(|queue, workspace| {
                    queue.admit_composer_operation_from_capture(
                        operation,
                        &authority,
                        pending,
                        ids,
                        |candidate| workspace.validate_queue_candidate(candidate),
                    )
                })?;
                workspace
                    .composer_documents
                    .submissions
                    .get_mut(id)
                    .expect("admission retains recovery data")
                    .admitted = true;
            }
            Ok(workspace
                .composer_documents
                .submissions
                .get(id)
                .expect("admission preserves capture bytes")
                .capture
                .clone())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    save_local(this, cx, id, permit).await?;
    if let Some(edit) = &capture.edit {
        let verified = this
            .update_in(cx, |workspace, _, _| {
                workspace
                    .queue_storage
                    .object(&edit.object)
                    .is_some_and(|object| object.verified())
            })
            .map_err(|_| QueueCaptureFailure::Unavailable)?;
        if !verified {
            let bytes = this
                .update_in(cx, |workspace, _, _| {
                    workspace
                        .composer_documents
                        .submissions
                        .get(id)
                        .and_then(|submission| submission.edit.as_ref())
                        .map(|edit| edit.bytes.clone())
                })
                .ok()
                .flatten()
                .ok_or(QueueCaptureFailure::Unavailable)?;
            queue_payload_io::persist_submission_edit(this, cx, id, bytes, explicit_retry, permit)
                .await?;
        }
    }
    // Publication is strictly local. The final source was constructed once with
    // the exact edit reference before atomic admission and is never regenerated.
    let source = this
        .update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(id, permit) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let operation = workspace
                .queue_storage
                .operation(id)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            match operation.source() {
                Some(QueueSource::Inline(_)) => operation
                    .compound_source_bytes()
                    .map(Some)
                    .map_err(Into::into),
                Some(QueueSource::Companion(_)) => Ok(None),
                None => Err(QueueCaptureFailure::Invalid),
            }
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    if let Some(bytes) = source {
        queue_payload_io::persist_submission_source(this, cx, id, bytes, explicit_retry, permit)
            .await?;
    }
    this.update_in(cx, |workspace, _, _| {
        if !workspace.submission_local_allowed(id, permit) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let owner = &capture.composer.native.owner;
        if let Some(edit) = &capture.edit {
            let current = workspace
                .composer_documents
                .documents
                .get(owner)
                .is_some_and(|document| {
                    (document.generation, document.revision)
                        == (capture.generation, capture.revision)
                        && document.known == capture.composer.references
                });
            if current
                && workspace.queue_storage.draft_companions(owner) == capture.composer.references
            {
                workspace.change_queue_storage(|queue, workspace| {
                    queue.publish_composer_edit(owner, edit, |candidate| {
                        workspace.validate_queue_candidate(candidate)
                    })
                })?;
            }
        }
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    save_local(this, cx, id, permit).await?;
    this.update_in(cx, |workspace, _, _| {
        if !workspace.submission_local_allowed(id, permit) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        if let Some(document) = workspace
            .composer_documents
            .documents
            .get_mut(&capture.composer.native.owner)
            .filter(|document| {
                (document.generation, document.revision) == (capture.generation, capture.revision)
            })
            && let Some(edit) = &capture.edit
        {
            document.durable = Some(edit.clone());
            if document.planned.as_ref().is_some_and(|planned| {
                (planned.generation, planned.revision) == (edit.generation, edit.revision)
            }) {
                document.planned = None;
            }
            if document.save_failure.as_ref().is_some_and(|failure| {
                failure.capture.generation == capture.generation
                    && failure.capture.revision <= capture.revision
            }) {
                document.save_failure = None;
            }
        }
        let submission = workspace
            .composer_documents
            .submissions
            .get_mut(id)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        submission.local_complete = true;
        submission.failed = false;
        submission.pending_io = false;
        submission.edit = None;
        Ok(submission.capture.clone())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?
}

async fn save_local(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    permit: Option<&SubmissionClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    let barrier = this
        .update_in(cx, |workspace, _, cx| {
            workspace
                .submission_local_allowed(id, permit)
                .then(|| workspace.persistence_barrier(cx))
                .flatten()
        })
        .ok()
        .flatten();
    queue_payload_io::durable(this, cx, barrier).await
}

async fn encode_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    permit: Option<&SubmissionClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    let capture = this
        .update_in(cx, |workspace, _, _| {
            if !workspace.submission_local_allowed(id, permit) {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let submission = workspace
                .composer_documents
                .submissions
                .get(id)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            Ok(if submission.edit.is_some() || submission.local_complete {
                None
            } else {
                Some(submission.capture.clone())
            })
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let Some(capture) = capture else {
        return Ok(());
    };
    let Some((text, selection, layout, shown)) = capture.snapshot.clone() else {
        return Ok(());
    };
    let operation = capture
        .composer
        .references
        .first()
        .ok_or(QueueCaptureFailure::Invalid)?
        .operation
        .clone();
    let object = match &capture.planned {
        Some(edit) => edit.object.clone(),
        None => QueueObjectId::new(&format!("{}-edit-{}", operation.as_str(), id.as_str()))?,
    };
    let existing = this
        .update_in(cx, |workspace, _, _| {
            workspace.queue_storage.object(&object).cloned()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?;
    let operation_id = id.clone();
    let encoded = cx
        .background_executor()
        .spawn(async move {
            let document = EditDocument {
                version: 1,
                owner: Owner::capture(&capture.composer.native.owner),
                generation: capture.generation,
                revision: capture.revision,
                companions: capture.composer.references.clone(),
                shown,
                text: text.to_string(),
                start: selection.start,
                end: selection.end,
                layout,
            };
            let bytes = serde_json::to_vec(&document).map_err(|_| QueueCaptureFailure::Invalid)?;
            let chunks = if let Some(existing) = &existing {
                existing
                    .reference()
                    .chunks()
                    .iter()
                    .map(|chunk| chunk.descriptor())
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                bytes
                    .chunks(QUEUE_CHUNK_BYTES)
                    .enumerate()
                    .map(|(ordinal, bytes)| {
                        let digest = sha256(
                            format!("native-submission:{}:edit:{ordinal}", operation_id.as_str())
                                .as_bytes(),
                        );
                        let label = digest[..16]
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>();
                        let label =
                            BlobId::parse(&label).map_err(|_| QueueCaptureFailure::Invalid)?;
                        Ok(BlobDescriptor::new(
                            label,
                            bytes.len() as u64,
                            sha256(bytes),
                        ))
                    })
                    .collect::<Result<Vec<_>, QueueCaptureFailure>>()?
            };
            let reference = QueueSpoolRef::new(
                bytes.len() as u64,
                sha256(&bytes),
                chunks,
                QueueObjectKind::NativeCompanion,
            )?;
            let edit = ComposerEditRef {
                operation: operation.clone(),
                object: object.clone(),
                generation: capture.generation,
                revision: capture.revision,
                byte_len: reference.total_bytes(),
                sha256: *reference.sha256(),
                companions: capture.composer.references.clone(),
            };
            document.validate(&capture.composer.native.owner, &edit)?;
            if capture
                .planned
                .as_ref()
                .is_some_and(|planned| planned != &edit)
                || existing
                    .as_ref()
                    .is_some_and(|existing| existing.reference() != &reference)
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            let object = if existing.is_some() {
                None
            } else {
                Some(QueueObject::planned(
                    object,
                    operation,
                    QueueObjectKind::NativeCompanion,
                    edit.generation,
                    reference,
                )?)
            };
            Ok::<_, QueueCaptureFailure>(EncodedEdit {
                reference: edit,
                object,
                bytes: Arc::new(bytes),
            })
        })
        .await?;
    this.update_in(cx, |workspace, _, _| {
        if !workspace.submission_local_allowed(id, permit) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let submission = workspace
            .composer_documents
            .submissions
            .get_mut(id)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        submission.capture.edit = Some(encoded.reference.clone());
        submission.capture.snapshot = None;
        submission.capture.planned = None;
        submission.edit = Some(encoded);
        Ok(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?
}
