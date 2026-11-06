//! Explicit staging of durable prepared attachments into fixed OpenCode targets.
//!
//! One task owns the entire read, plan, write and final-save chain. Waiting entries
//! contain identities only. Intake, spool puts, removal and send materialization
//! belong to their existing owners. A restored target is used only by a new
//! explicit activation, and every failure retains its original URI and bytes.
//!
//! Close must call `cancel_attachment_staging` before its final journal snapshot,
//! or hold the journal open until `is_busy` is false. Retired network collection
//! must honor `holds_runtime` while this task awaits disk receipts. Consumers use
//! `prepared_attachment_ready`, which includes the final durability wait.

use std::collections::VecDeque;

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::{DraftKey, DraftStore};
use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
use openchamber_native_core::ids::{AttachmentId, RuntimeId};
use openchamber_native_transport::attachment::{AttachmentPlan, AttachmentUpload};
use openchamber_native_transport::client::ServerClient;

use crate::attachment_catalog::{
    self, AttachmentCatalog, PreparedAttachment, StagingDescriptor, StagingPhase,
};
use crate::connection::ConnectionState;
use crate::journal_worker::{JournalFailure, JournalReceipt};
use crate::mutation_journal::MAX_ENTRIES;
use crate::network::NetworkTask;
use crate::state_persistence::WriteStamp;
use crate::workspace::Workspace;

const FAILED: &str = "chat.chatInput.toast.attachFileFailed";

#[derive(Clone, PartialEq, Eq)]
struct StagingRequest {
    owner: DraftKey,
    id: AttachmentId,
}

#[derive(Clone, PartialEq, Eq)]
struct StagingJob {
    generation: u64,
    canceled: bool,
    request: StagingRequest,
    file: DraftFile,
    prepared: PreparedAttachment,
}

impl StagingJob {
    fn still_owned(&self, drafts: &DraftStore, catalog: &AttachmentCatalog) -> bool {
        owned_prepared(drafts, catalog, &self.request.owner, &self.request.id)
            .is_some_and(|(file, prepared)| file == &self.file && prepared == &self.prepared)
    }

    fn accepts_target(&self, target: &StagingDescriptor) -> bool {
        if target.runtime != *self.request.owner.runtime()
            || attachment_catalog::remote_path(&target.directory).as_deref()
                != Some(self.request.owner.directory())
            || self.file.name.as_deref() != Some(target.filename.as_str())
            || target.uri != attachment_catalog::file_uri(&target.server_path)
        {
            return false;
        }
        self.prepared.staging.as_ref().is_none_or(|retained| {
            let mut expected = retained.clone();
            expected.phase = target.phase;
            let Some(directory) = attachment_catalog::remote_path(&expected.directory) else {
                return false;
            };
            let Some(root) = attachment_catalog::remote_path(&expected.temporary_directory) else {
                return false;
            };
            expected.directory = directory;
            expected.temporary_directory = root;
            target == &expected
        })
    }
}

/// Window-owned work. No queued entry owns an attachment buffer or a client.
#[derive(Default)]
pub(crate) struct AttachmentStaging {
    pending: VecDeque<StagingRequest>,
    active: Option<StagingJob>,
    generation: u64,
    task: Option<Task<()>>,
}

impl AttachmentStaging {
    /// Includes the final Ready save, so close cannot seal its journal too early.
    #[cfg(test)]
    pub(crate) fn is_busy(&self) -> bool {
        self.active.is_some() || !self.pending.is_empty() || self.task.is_some()
    }

    fn contains(&self, owner: &DraftKey, id: &AttachmentId) -> bool {
        self.active
            .as_ref()
            .is_some_and(|job| &job.request.owner == owner && &job.request.id == id)
            || self
                .pending
                .iter()
                .any(|request| &request.owner == owner && &request.id == id)
    }

    pub(crate) fn stages_id(&self, id: &AttachmentId) -> bool {
        self.active
            .as_ref()
            .is_some_and(|job| &job.request.id == id)
            || self.pending.iter().any(|request| &request.id == id)
    }

    /// A captured Handle does not itself keep its original Tokio runtime alive.
    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.active
            .as_ref()
            .is_some_and(|job| job.request.owner.runtime() == runtime)
    }

    fn enqueue(&mut self, request: StagingRequest) -> bool {
        if self.contains(&request.owner, &request.id) {
            return true;
        }
        if self.pending.len() + usize::from(self.active.is_some()) >= MAX_ENTRIES {
            return false;
        }
        self.pending.push_back(request);
        true
    }

    fn invalidate(&mut self, owner: &DraftKey, id: Option<&AttachmentId>) {
        let matches = |request: &StagingRequest| {
            &request.owner == owner && id.is_none_or(|id| &request.id == id)
        };
        self.pending.retain(|request| !matches(request));
        if let Some(active) = &mut self.active
            && matches(&active.request)
        {
            // Keep the executor held and the one-file slot occupied until the
            // existing await drains. Even re-adding an identical value cannot
            // give that retired completion authority again.
            active.canceled = true;
        }
    }
}

struct CapturedServer {
    client: ServerClient,
    executor: tokio::runtime::Handle,
    generation: u64,
}

impl CapturedServer {
    fn still_connected(&self, workspace: &Workspace, owner: &DraftKey) -> bool {
        workspace.runtime == *owner.runtime()
            && workspace.connection.as_ref().is_some_and(|connection| {
                connection.state == ConnectionState::Connected
                    && connection.generation == self.generation
                    && connection
                        .client
                        .as_ref()
                        .is_some_and(|client| client.endpoint() == self.client.endpoint())
            })
    }
}

type SaveBarrier = (WriteStamp, JournalReceipt);

impl Workspace {
    /// Removal calls with Some(id); whole-draft discard calls with None. Call
    /// before removing the owned values, including an already-absent removal.
    pub(crate) fn invalidate_attachment_staging(
        &mut self,
        owner: &DraftKey,
        id: Option<&AttachmentId>,
        cx: &mut Context<Self>,
    ) {
        self.attachment_staging.invalidate(owner, id);
        cx.notify();
    }

    /// Readiness for presentation and outgoing materialization. The catalog can
    /// temporarily contain Ready while its snapshot is being saved. Every owner
    /// of the same immutable attachment must wait for that save to settle.
    pub(crate) fn prepared_attachment_ready(&self, owner: &DraftKey, id: &AttachmentId) -> bool {
        self.persistence_ready()
            && !self.attachment_staging.stages_id(id)
            && owned_prepared(&self.drafts, &self.attachments, owner, id).is_some_and(
                |(_, prepared)| {
                    prepared
                        .staging
                        .as_ref()
                        .is_some_and(|target| target.phase == StagingPhase::Ready)
                },
            )
    }

    /// Explicit entry after intake has durably published both catalog and DraftFile.
    /// Repeating this activation while that owner/ID is pending is a no-op.
    pub(crate) fn stage_prepared_attachment(
        &mut self,
        owner: DraftKey,
        id: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_in_progress() || !self.persistence_loaded() {
            return;
        }
        if !self
            .attachment_staging
            .enqueue(StagingRequest { owner, id })
        {
            self.notify_mutation_failure(FAILED, window, cx);
            return;
        }
        self.start_attachment_staging(window, cx);
    }

    fn start_attachment_staging(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.attachment_staging.task.is_some() {
            return;
        }
        self.attachment_staging.task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let next = this.update_in(cx, |this, window, cx| {
                    this.begin_attachment_staging(window, cx)
                });
                let Ok(Some((mut job, server, (stamp, saved)))) = next else {
                    break;
                };
                // This block drops every read reply/plan before the next job can
                // acquire a buffer. Even failures at the front of a batch release
                // only their own job and allow the remaining metadata to proceed.
                let acknowledged = 'file: {
                    let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
                    let read = this
                        .update_in(cx, |this, _, cx| {
                            let durable = this.record_persistence(stamp, &result, cx);
                            if !durable
                                || !this.attachment_job_current(&job)
                                || this.close_in_progress()
                                || !server.still_connected(this, &job.request.owner)
                            {
                                return None;
                            }
                            this.attachment_worker()
                                .map(|worker| worker.read_attachment(job.prepared.blob))
                        })
                        .ok()
                        .flatten();
                    let Some(read) = read else {
                        break 'file false;
                    };
                    let Ok(Ok(bytes)) = read.await else {
                        break 'file false;
                    };
                    let planned = this
                        .update_in(cx, |this, _, _| {
                            if !this.attachment_job_current(&job)
                                || this.close_in_progress()
                                || !server.still_connected(this, &job.request.owner)
                            {
                                return None;
                            }
                            let client = server.client.clone();
                            let directory = job.request.owner.directory().to_owned();
                            let filename = job.file.name.clone()?;
                            let retained = job.prepared.staging.clone();
                            Some(NetworkTask::spawn_on(&server.executor, async move {
                                // BlobBytes has no consuming Vec accessor. Copy on
                                // the network executor, then release the read buffer
                                // before any HTTP request. The worker verified SHA.
                                let file =
                                    AttachmentUpload::new(&filename, bytes.as_slice().to_vec());
                                drop(bytes);
                                let file = file?;
                                match retained {
                                    Some(target) => {
                                        client
                                            .restore_attachment_plan(
                                                &directory,
                                                &target.temporary_directory,
                                                &target.server_path,
                                                file,
                                            )
                                            .await
                                    }
                                    None => client.plan_attachment(&directory, file).await,
                                }
                            }))
                        })
                        .ok()
                        .flatten();
                    let Some(planned) = planned else {
                        break 'file false;
                    };
                    let Some(Ok(plan)) = planned.await else {
                        break 'file false;
                    };
                    let writing = this
                        .update_in(cx, |this, _, cx| {
                            if !this.attachment_job_current(&job)
                                || this.close_in_progress()
                                || !server.still_connected(this, &job.request.owner)
                            {
                                return None;
                            }
                            let target = writing_target(&job, &plan)?;
                            this.set_attachment_target(&mut job, target);
                            // Writing and the fixed URI share one snapshot. There
                            // is no separate redundant Planned save barrier.
                            this.persistence_barrier(cx)
                        })
                        .ok()
                        .flatten();
                    let Some((stamp, saved)) = writing else {
                        break 'file false;
                    };
                    let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
                    let writing = this
                        .update_in(cx, |this, _, cx| {
                            let durable = this.record_persistence(stamp, &result, cx);
                            if !durable
                                || !this.attachment_job_current(&job)
                                || this.close_in_progress()
                                || !server.still_connected(this, &job.request.owner)
                            {
                                return None;
                            }
                            // Dispatch under the ownership/close check, using the
                            // original client and executor held inside this plan.
                            Some(NetworkTask::spawn_on(&server.executor, async move {
                                plan.write().await
                            }))
                        })
                        .ok()
                        .flatten();
                    let Some(writing) = writing else {
                        break 'file false;
                    };
                    let result = writing.await;
                    matches!(result, Some(Ok(ref receipt)) if job.prepared.staging.as_ref()
                        .is_some_and(|target| receipt.server_path() == target.server_path))
                };
                let final_save = this
                    .update_in(cx, |this, window, cx| {
                        this.finish_attachment_write(&mut job, acknowledged, window, cx)
                    })
                    .ok()
                    .flatten();
                if let Some((stamp, saved)) = final_save {
                    let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
                    let _ = this.update_in(cx, |this, window, cx| {
                        let durable = this.record_persistence(stamp, &result, cx);
                        if !durable
                            && !this.close_in_progress()
                            && this.attachment_job_current(&job)
                        {
                            // The readiness accessor withholds Ready during this wait.
                            // A failed save retains the fixed target for explicit
                            // retry, without scheduling a second automatic save.
                            this.set_attachment_phase(&mut job, StagingPhase::Uncertain);
                            if acknowledged {
                                this.notify_mutation_failure(FAILED, window, cx);
                            }
                        }
                    });
                }
                let _ = this.update_in(cx, |this, _, cx| {
                    if this
                        .attachment_staging
                        .active
                        .as_ref()
                        .is_some_and(|active| active.generation == job.generation)
                    {
                        this.attachment_staging.active = None;
                        cx.notify();
                    }
                });
            }
        }));
        cx.notify();
    }

    fn begin_attachment_staging(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(StagingJob, CapturedServer, SaveBarrier)> {
        if self.close_in_progress() {
            self.attachment_staging.pending.clear();
        }
        while let Some(request) = self.attachment_staging.pending.pop_front() {
            let Some((file, prepared)) =
                owned_prepared(&self.drafts, &self.attachments, &request.owner, &request.id)
            else {
                continue;
            };
            if prepared
                .staging
                .as_ref()
                .is_some_and(|target| target.phase == StagingPhase::Ready)
            {
                continue;
            }
            let connection = self.connection.as_ref().filter(|connection| {
                self.runtime == *request.owner.runtime()
                    && connection.state == ConnectionState::Connected
            });
            let Some((connection, client)) = connection.and_then(|connection| {
                connection.client.clone().map(|client| (connection, client))
            }) else {
                self.notify_mutation_failure(FAILED, window, cx);
                continue;
            };
            let server = CapturedServer {
                client,
                executor: connection.network.handle(),
                generation: connection.generation,
            };
            let Some(generation) = self.attachment_staging.generation.checked_add(1) else {
                self.notify_mutation_failure(FAILED, window, cx);
                continue;
            };
            let job = StagingJob {
                generation,
                canceled: false,
                request,
                file: file.clone(),
                prepared: prepared.clone(),
            };
            self.attachment_staging.generation = generation;
            let Some(barrier) = self.persistence_barrier(cx) else {
                self.notify_mutation_failure(FAILED, window, cx);
                continue;
            };
            self.attachment_staging.active = Some(job.clone());
            return Some((job, server, barrier));
        }
        self.attachment_staging.task = None;
        None
    }

    fn attachment_job_current(&self, job: &StagingJob) -> bool {
        self.attachment_staging.active.as_ref() == Some(job)
            && job.still_owned(&self.drafts, &self.attachments)
    }

    fn set_attachment_target(&mut self, job: &mut StagingJob, target: StagingDescriptor) {
        // All callers first validate the complete current job. No insertion can
        // resurrect an attachment removed while a disk or network await ran.
        self.attachments
            .get_mut(&job.request.id)
            .expect("current staging job retains its catalog entry")
            .staging = Some(target.clone());
        job.prepared.staging = Some(target);
        self.attachment_staging.active = Some(job.clone());
    }

    fn set_attachment_phase(&mut self, job: &mut StagingJob, phase: StagingPhase) {
        if let Some(mut target) = job.prepared.staging.clone() {
            target.phase = phase;
            self.set_attachment_target(job, target);
        }
    }

    fn finish_attachment_write(
        &mut self,
        job: &mut StagingJob,
        acknowledged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<SaveBarrier> {
        if !self.attachment_job_current(job) || self.close_in_progress() {
            return None;
        }
        if !acknowledged {
            self.notify_mutation_failure(FAILED, window, cx);
        }
        let target = job.prepared.staging.as_ref()?;
        let phase = if acknowledged {
            StagingPhase::Ready
        } else {
            StagingPhase::Uncertain
        };
        if target.phase == phase {
            return None;
        }
        self.set_attachment_phase(job, phase);
        let barrier = self.persistence_barrier(cx);
        if barrier.is_none() {
            self.set_attachment_phase(job, StagingPhase::Uncertain);
        }
        cx.notify();
        barrier
    }

    /// Cancel before starting close's final snapshot or quit flush. Dropping the
    /// task aborts local network I/O only; the server may already have its bytes.
    /// The retained target remains recoverable under Writing/Uncertain on disk.
    pub(crate) fn cancel_attachment_staging(&mut self, cx: &mut Context<Self>) {
        self.attachment_staging.pending.clear();
        self.attachment_staging.task = None;
        if let Some(mut job) = self.attachment_staging.active.clone()
            && self.attachment_job_current(&job)
        {
            self.set_attachment_phase(&mut job, StagingPhase::Uncertain);
        }
        self.attachment_staging.active = None;
        cx.notify();
    }
}

fn owned_prepared<'a>(
    drafts: &'a DraftStore,
    catalog: &'a AttachmentCatalog,
    owner: &DraftKey,
    id: &AttachmentId,
) -> Option<(&'a DraftFile, &'a PreparedAttachment)> {
    let mut files = drafts
        .get(owner)?
        .content
        .files
        .iter()
        .filter(|file| &file.id == id);
    let file = files.next()?;
    let prepared = catalog.get(id)?;
    if files.next().is_some()
        || !matches!(&file.content, DraftFileContent::Prepared)
        || file.byte_len != Some(prepared.blob.byte_len())
        || prepared.blob.id().as_str() != id.as_str()
        || file.name.is_none()
        || prepared.staging.as_ref().is_some_and(|target| {
            target.runtime != *owner.runtime()
                || attachment_catalog::remote_path(&target.directory).as_deref()
                    != Some(owner.directory())
                || file.name.as_deref() != Some(target.filename.as_str())
        })
    {
        return None;
    }
    Some((file, prepared))
}

fn writing_target(job: &StagingJob, plan: &AttachmentPlan) -> Option<StagingDescriptor> {
    let target = StagingDescriptor {
        runtime: job.request.owner.runtime().clone(),
        directory: plan.directory().to_owned(),
        temporary_directory: plan.temporary_directory().to_owned(),
        server_path: plan.server_path().to_owned(),
        uri: attachment_catalog::file_uri(plan.server_path()),
        filename: plan.filename().to_owned(),
        phase: StagingPhase::Writing,
    };
    if !job.accepts_target(&target) || plan.bytes().len() as u64 != job.prepared.blob.byte_len() {
        return None;
    }
    // Restoration normalizes the root for validation but keeps the target's
    // original spelling. Keep the complete saved descriptor equally unchanged.
    let mut target = job.prepared.staging.clone().unwrap_or(target);
    target.phase = StagingPhase::Writing;
    Some(target)
}

#[cfg(test)]
#[path = "attachment_staging_tests.rs"]
mod tests;
