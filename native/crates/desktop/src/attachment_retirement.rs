//! Explicit local-byte retirement after a user removes an attachment owner.
//!
//! Only the named identity is considered. No directory scans, startup cleanup or
//! server-file deletion occur. A durable intent precedes unlinking and survives
//! every failed or uncertain outcome for an explicit repeat.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::{Draft, DraftStore};
use openchamber_native_core::ids::AttachmentId;

use crate::attachment_catalog::AttachmentCatalog;
use crate::attachment_catalog::PreparationHold;
use crate::attachment_spool::{BlobId, SpoolCommit};
use crate::create_attempt::CreateAttempts;
use crate::journal_worker::JournalFailure;
use crate::mutation_journal::MAX_ENTRIES;
use crate::send_attempt::SendAttempts;
use crate::starter_capture::StarterCaptures;
use crate::workspace::Workspace;

#[derive(Default)]
pub(crate) struct AttachmentRetirement {
    tasks: HashMap<BlobId, Task<()>>,
    /// Named obligations whose catalog removal has not committed yet. Entries
    /// belong to the bounded catalog/retirement namespaces, never a spool scan.
    pending: HashSet<AttachmentId>,
}

impl AttachmentRetirement {
    pub(crate) fn busy(&self, blob: &BlobId) -> bool {
        self.tasks.contains_key(blob)
    }

    pub(crate) fn cancel(&mut self) {
        // Accepted spool operations drain in the serialized worker. Their intent
        // remains in the final snapshot even when the UI waiter is canceled.
        self.tasks.clear();
    }
}

impl Workspace {
    /// Retry only captured roots after their ephemeral owner has released them.
    pub(crate) fn retire_released_native_roots(
        &mut self,
        roots: Vec<AttachmentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prune_settled_attachment_retirements();
        for id in roots {
            self.start_attachment_retirement(id, window, cx);
        }
        cx.notify();
    }

    /// The Retry action: pending releases, and journaled retirements whose
    /// earlier release failed. Close retries only the pending set, so a failed
    /// unlink keeps its durable intent without blocking the window.
    pub(crate) fn retry_attachment_retirements(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let committed: Vec<AttachmentId> = self
            .retired_attachment_blobs
            .iter()
            .filter(|blob| !self.attachment_retirement.tasks.contains_key(*blob))
            .map(|blob| AttachmentId::from(blob.as_str()))
            .filter(|id| !self.attachment_has_owner(id) && !self.attachments.contains_key(id))
            .collect();
        for id in committed {
            self.start_attachment_retirement(id, window, cx);
        }
        self.retry_pending_attachment_retirements(window, cx);
    }

    /// Explicit retry for named releases deferred by close, storage or capacity.
    pub(crate) fn retry_pending_attachment_retirements(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prune_settled_attachment_retirements();
        let pending: Vec<_> = self.attachment_retirement.pending.iter().cloned().collect();
        for id in pending {
            self.start_attachment_retirement(id, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn has_pending_attachment_retirements(&self) -> bool {
        !self.attachment_retirement.pending.is_empty()
    }

    fn prune_settled_attachment_retirements(&mut self) {
        let mut pending = std::mem::take(&mut self.attachment_retirement.pending);
        pending.retain(|id| {
            let Ok(blob) = BlobId::parse(id.as_str()) else {
                return false;
            };
            !self.attachment_has_owner(id)
                && (self
                    .attachments
                    .get(id)
                    .is_some_and(|prepared| prepared.blob.id() == &blob)
                    || self.retired_attachment_blobs.contains(&blob))
        });
        self.attachment_retirement.pending = pending;
    }

    pub(crate) fn retire_attachment(
        &mut self,
        id: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.retire_released_native_roots(vec![id], window, cx);
    }

    fn start_attachment_retirement(
        &mut self,
        id: AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.attachment_has_owner(&id) {
            self.attachment_retirement.pending.remove(&id);
            return;
        }
        let Ok(blob) = BlobId::parse(id.as_str()) else {
            self.attachment_retirement.pending.remove(&id);
            return;
        };
        if !self
            .attachments
            .get(&id)
            .is_some_and(|prepared| prepared.blob.id() == &blob)
            && !self.retired_attachment_blobs.contains(&blob)
        {
            self.attachment_retirement.pending.remove(&id);
            return;
        }
        self.attachment_retirement.pending.insert(id.clone());
        if self.close_in_progress()
            || !self.persistence_loaded()
            || self.attachment_worker().is_none()
        {
            return;
        }
        if self.attachment_retirement.tasks.contains_key(&blob) {
            return;
        }
        match begin_retirement(
            &mut self.attachments,
            &mut self.retired_attachment_blobs,
            &id,
            blob,
        ) {
            RetirementStart::Ready => {}
            RetirementStart::NotOwned => return,
            RetirementStart::AtCapacity => {
                self.notify_mutation_failure("chat.chatInput.toast.attachFileFailed", window, cx);
                return;
            }
        }
        let Some((stamp, saved)) = self.persistence_barrier(cx) else {
            return;
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let release = this
                .update_in(cx, |this, _, cx| {
                    let committed = this.record_persistence(stamp, &result, cx);
                    if committed {
                        // The existing journaled intent now owns this release,
                        // including cancellation or unlink failure after this save.
                        this.attachment_retirement.pending.remove(&id);
                    }
                    if !committed
                        || this.close_in_progress()
                        || this.attachment_has_owner(&id)
                        || this.attachments.contains_key(&id)
                        || !this.retired_attachment_blobs.contains(&blob)
                    {
                        return None;
                    }
                    this.attachment_worker()
                        .map(|worker| worker.release_attachment(blob))
                })
                .ok()
                .flatten();
            let durable = match release {
                Some(release) => matches!(release.await, Ok(Ok(SpoolCommit::Durable))),
                None => false,
            };
            let final_save = this
                .update_in(cx, |this, window, cx| {
                    if durable
                        && !this.attachment_has_owner(&id)
                        && !this.attachments.contains_key(&id)
                    {
                        this.retired_attachment_blobs
                            .retain(|retired| retired != &blob);
                        this.persistence_barrier(cx)
                    } else {
                        if !this.close_in_progress() {
                            this.notify_mutation_failure(
                                "chat.chatInput.toast.attachFileFailed",
                                window,
                                cx,
                            );
                        }
                        None
                    }
                })
                .ok()
                .flatten();
            if let Some((stamp, saved)) = final_save {
                let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
                let _ = this.update_in(cx, |this, _, cx| {
                    this.record_persistence(stamp, &result, cx);
                });
            }
            let _ = this.update_in(cx, |this, _, cx| {
                this.attachment_retirement.tasks.remove(&blob);
                cx.notify();
            });
        });
        self.attachment_retirement.tasks.insert(blob, task);
    }

    pub(crate) fn attachment_has_owner(&self, id: &AttachmentId) -> bool {
        self.queue_storage.references_native(id)
            || self.queue_storage.references_captured_native(id)
            || self.queue.enqueues.references_native(id)
            || self.queue.prefixes.references_native(id)
            || self.queue.local_sends.references_native(id)
            || referenced(
                id,
                &self.drafts,
                &self.send_attempts,
                &self.creations,
                &self.starters.captures,
                &self.attachment_preparations,
            )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetirementStart {
    Ready,
    NotOwned,
    AtCapacity,
}

pub(crate) fn begin_retirement(
    catalog: &mut AttachmentCatalog,
    retired: &mut Vec<BlobId>,
    id: &AttachmentId,
    blob: BlobId,
) -> RetirementStart {
    let Some(prepared) = catalog.get(id) else {
        return if retired.contains(&blob) {
            RetirementStart::Ready
        } else {
            RetirementStart::NotOwned
        };
    };
    if prepared.blob.id() != &blob {
        return RetirementStart::NotOwned;
    }
    if !retired.contains(&blob) && retired.len() >= MAX_ENTRIES {
        return RetirementStart::AtCapacity;
    }
    catalog.remove(id);
    if !retired.contains(&blob) {
        retired.push(blob)
    }
    RetirementStart::Ready
}

fn in_draft(draft: &Draft, id: &AttachmentId) -> bool {
    draft
        .content
        .files
        .iter()
        .any(|file| &file.id == id || file.source_document_id.as_ref() == Some(id))
}

pub(crate) fn referenced(
    id: &AttachmentId,
    drafts: &DraftStore,
    sends: &SendAttempts,
    creations: &CreateAttempts,
    starters: &StarterCaptures,
    preparations: &[PreparationHold],
) -> bool {
    preparations.iter().any(|hold| &hold.id == id)
        || drafts.iter().any(|(_, draft)| in_draft(draft, id))
        || sends
            .iter()
            .any(|(_, attempt)| in_draft(&attempt.draft, id) || attempt.resources.contains(id))
        || creations
            .iter()
            .any(|(_, attempt)| in_draft(&attempt.draft, id) || attempt.resources.contains(id))
        || starters.iter().any(|capture| in_draft(&capture.draft, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::draft::{DraftKey, DraftTarget};
    use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
    use openchamber_native_core::ids::{DraftId, RuntimeId};

    #[test]
    fn full_retirement_capacity_preserves_the_catalog_and_existing_drain_authority() {
        use crate::attachment_catalog::PreparedAttachment;
        use crate::attachment_spool::BlobDescriptor;

        let id = AttachmentId::from("ffffffffffffffffffffffffffffffff");
        let blob = BlobId::parse(id.as_str()).expect("blob");
        let prepared = PreparedAttachment {
            blob: BlobDescriptor::new(blob, 3, [7; 32]),
            staging: None,
        };
        let mut catalog = AttachmentCatalog::from_iter([(id.clone(), prepared.clone())]);
        let mut retired: Vec<_> = (0..MAX_ENTRIES)
            .map(|index| BlobId::parse(&format!("{index:032x}")).expect("unique label"))
            .collect();
        crate::attachment_catalog::validate(&mut 0, &catalog, &[], &retired)
            .expect("valid hydrated state");
        assert_eq!(
            begin_retirement(&mut catalog, &mut retired, &id, blob),
            RetirementStart::AtCapacity
        );
        assert_eq!(
            catalog.get(&id),
            Some(&prepared),
            "capacity refusal keeps byte authority intact"
        );
        let first = retired[0];
        assert_eq!(
            begin_retirement(
                &mut catalog,
                &mut retired,
                &AttachmentId::from(first.as_str()),
                first
            ),
            RetirementStart::Ready
        );
        crate::attachment_catalog::validate(&mut 0, &catalog, &[], &retired)
            .expect("unrelated saves and existing drains remain valid");
        retired.pop();
        assert_eq!(
            begin_retirement(&mut catalog, &mut retired, &id, blob),
            RetirementStart::Ready
        );
        assert_eq!(retired.len(), MAX_ENTRIES);
        assert!(!catalog.contains_key(&id));
        crate::attachment_catalog::validate(&mut 0, &catalog, &[], &retired)
            .expect("the accepted transition remains within bounds");
    }

    #[test]
    fn retirement_keeps_inactive_draft_and_pre_put_roots() {
        let id = AttachmentId::from("0123456789abcdef0123456789abcdef");
        let blob = BlobId::parse(id.as_str()).expect("blob label");
        let owner = DraftKey::new(
            RuntimeId::from("inactive"),
            "/project",
            DraftTarget::NewSession(DraftId::from("draft")),
        )
        .expect("owner");
        let mut draft = Draft::default();
        draft.content.files.push(DraftFile {
            id: id.clone(),
            name: Some("retained.txt".into()),
            media_type: Some("text/plain".into()),
            byte_len: Some(3),
            source_document_id: None,
            content: DraftFileContent::Prepared,
        });
        let mut drafts = DraftStore::new();
        drafts.save(owner.clone(), draft);
        let sends = SendAttempts::default();
        let creations = CreateAttempts::default();
        let starters = StarterCaptures::default();
        assert!(referenced(&id, &drafts, &sends, &creations, &starters, &[]));
        drafts.take(&owner);
        let hold = PreparationHold {
            owner,
            id: id.clone(),
            generation: 1,
            blob,
        };
        assert!(referenced(
            &id,
            &drafts,
            &sends,
            &creations,
            &starters,
            &[hold]
        ));
        assert!(!referenced(
            &id,
            &drafts,
            &sends,
            &creations,
            &starters,
            &[]
        ));
    }

    #[cfg(unix)]
    mod released_roots {
        use super::*;

        use gpui_kit::{AppContext, TestAppContext};
        use openchamber_native_core::ids::SessionId;

        use crate::attachment_catalog::PreparedAttachment;
        use crate::attachment_io::AttachmentJournal;
        use crate::attachment_spool::BlobDescriptor;
        use crate::journal_worker::CommitOutcome;
        use crate::mutation_journal::{JournalCommit, JournalSnapshot, LoadedJournal};
        use crate::queue_capture::sha256;

        fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
            assert!(matches!(
                journal.save(snapshot).expect("canonical fixture snapshot"),
                JournalCommit::Durable
            ));
        }

        fn seed(journal: &mut AttachmentJournal) -> (JournalSnapshot, Vec<BlobDescriptor>) {
            let owner = DraftKey::new(
                RuntimeId::from("retirement-host"),
                "/project",
                DraftTarget::Session(SessionId::from("ses_retirement")),
            )
            .expect("exact sibling owner");
            let holds: Vec<_> = (0x8001..=0x8003)
                .map(|value| {
                    let blob = BlobId::parse(&format!("{value:032x}")).expect("private file label");
                    PreparationHold {
                        owner: owner.clone(),
                        id: AttachmentId::from(blob.as_str()),
                        generation: 1,
                        blob,
                    }
                })
                .collect();
            let mut snapshot = JournalSnapshot {
                preparations: holds.clone(),
                ..JournalSnapshot::default()
            };
            save(journal, &snapshot);
            let mut descriptors = Vec::new();
            for hold in &holds {
                let receipt = journal
                    .put(hold, b"original")
                    .expect("actual committed prepared bytes");
                descriptors.push(receipt.descriptor);
                snapshot.attachments.insert(
                    hold.id.clone(),
                    PreparedAttachment {
                        blob: receipt.descriptor,
                        staging: None,
                    },
                );
            }
            snapshot.preparations.clear();
            let mut sibling = Draft::new("independent sibling owner");
            sibling.content.files.push(DraftFile {
                id: holds[1].id.clone(),
                name: Some("shared.txt".into()),
                media_type: Some("text/plain".into()),
                byte_len: Some(8),
                source_document_id: None,
                content: DraftFileContent::Prepared,
            });
            snapshot.drafts.push((owner, sibling));
            save(journal, &snapshot);
            (snapshot, descriptors)
        }

        #[gpui_kit::test]
        async fn named_release_waits_for_storage_close_and_capacity_then_retries_without_sweeping_siblings(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let fixture = crate::recovery_tests::Fixture::new();
            let (mut journal, _) =
                AttachmentJournal::open(&fixture.journal()).expect("private journal");
            let (mut snapshot, descriptors) = seed(&mut journal);
            snapshot.retired_blobs = (10_000..10_000 + MAX_ENTRIES)
                .map(|value| {
                    BlobId::parse(&format!("{value:032x}")).expect("existing named intent")
                })
                .collect();
            save(&mut journal, &snapshot);
            drop(journal);
            let removed = AttachmentId::from(descriptors[0].id().as_str());
            let shared = AttachmentId::from(descriptors[1].id().as_str());
            let unnamed = AttachmentId::from(descriptors[2].id().as_str());
            let logical = AttachmentId::from("logical-source-group");
            let missing = AttachmentId::from("00000000000000000000000000008fff");
            let (window, workspace) = crate::workspace_tests::open(cx);
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.composer_key = None;
                    workspace.attachments = snapshot.attachments.clone();
                    workspace.retire_released_native_roots(
                        vec![removed.clone(), logical.clone(), missing.clone()],
                        window,
                        cx,
                    );
                    assert!(
                        workspace.has_pending_attachment_retirements(),
                        "unavailable storage retains the named catalog-backed request"
                    );
                    assert_eq!(
                        workspace.attachment_retirement.pending.len(),
                        1,
                        "logical and missing names grant no descriptor or retirement intent"
                    );
                    assert!(
                        workspace.attachments.contains_key(&removed)
                            && workspace.retired_attachment_blobs.is_empty()
                    );
                    workspace.open_persistence(fixture.journal(), window, cx);
                })
            })
            .expect("capture the named obligation before storage is ready");
            cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
                .await;
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    assert!(
                        workspace.has_pending_attachment_retirements(),
                        "hydration never retries the release automatically"
                    );
                    workspace.general_close = Some(Task::ready(()));
                    workspace.retire_released_native_roots(
                        vec![removed.clone(), shared.clone(), logical, missing],
                        window,
                        cx,
                    );
                    workspace.attachment_retirement.cancel();
                    workspace.retry_pending_attachment_retirements(window, cx);
                    assert!(
                        workspace.has_pending_attachment_retirements(),
                        "close and task cancellation preserve the unwritten named request"
                    );
                    assert_eq!(workspace.attachment_retirement.pending.len(), 1);
                    assert!(
                        workspace.attachments.contains_key(&removed)
                            && workspace.attachments.contains_key(&shared)
                    );
                    assert!(
                        !workspace
                            .retired_attachment_blobs
                            .contains(descriptors[0].id())
                    );
                    assert!(!workspace.attachment_retirement.busy(descriptors[0].id()));
                    workspace.general_close = None;
                    workspace.retry_pending_attachment_retirements(window, cx);
                    assert!(
                        workspace.has_pending_attachment_retirements(),
                        "capacity refusal retains explicit retry ownership"
                    );
                    assert_eq!(workspace.retired_attachment_blobs.len(), MAX_ENTRIES);
                    assert!(workspace.attachments.contains_key(&removed));
                    assert!(!workspace.attachment_retirement.busy(descriptors[0].id()));
                })
            })
            .expect("close and capacity cannot discard or perform the named release");
            let first = snapshot.retired_blobs[0];
            let release = workspace.update(cx, |workspace, _| {
                workspace
                    .attachment_worker()
                    .expect("real file owner")
                    .release_attachment(first)
            });
            assert_eq!(
                release.await.expect("existing intent release answers"),
                Ok(SpoolCommit::Durable)
            );
            cx.update_window(window, |_, window, cx| workspace.update(cx, |workspace, cx| {
                // The actual file owner has now settled this existing intent.
                workspace.retired_attachment_blobs.retain(|blob| blob != &first);
                workspace.retry_pending_attachment_retirements(window, cx);
                assert!(!workspace.attachments.contains_key(&removed));
                assert!(workspace.retired_attachment_blobs.contains(descriptors[0].id()));
                assert!(workspace.has_pending_attachment_retirements(), "a submitted save still cannot grant durable release authority");
                assert!(workspace.attachment_retirement.busy(descriptors[0].id()));
                workspace.attachment_retirement.cancel();
                assert!(workspace.has_pending_attachment_retirements() && !workspace.attachment_retirement.busy(descriptors[0].id()),
                    "canceling an unconsumed save waiter must preserve its named release obligation");
                workspace.retry_pending_attachment_retirements(window, cx);
                assert!(workspace.attachment_retirement.busy(descriptors[0].id()), "an explicit retry can save the same existing intent again");
            })).expect("one freed slot accepts the explicit named retry");
            cx.condition(&workspace, |workspace, _| {
                !workspace.attachment_retirement.busy(descriptors[0].id())
            })
            .await;
            let (stamp, saved) = workspace.update(cx, |workspace, cx| {
                assert!(!workspace.has_pending_attachment_retirements());
                assert!(
                    !workspace
                        .retired_attachment_blobs
                        .contains(descriptors[0].id())
                );
                assert!(
                    workspace.attachments.contains_key(&shared)
                        && workspace.attachments.contains_key(&unnamed)
                );
                workspace
                    .persistence_barrier(cx)
                    .expect("complete final snapshot")
            });
            let saved = saved.await.expect("final snapshot answers");
            assert!(workspace.update(cx, |workspace, cx| {
                workspace.record_persistence(stamp, &saved, cx)
            }));
            let finish = workspace.update(cx, |workspace, _| {
                workspace.attachment_worker().expect("real worker").finish()
            });
            assert_eq!(
                finish.await.expect("worker drain"),
                Ok(CommitOutcome::Durable)
            );
            cx.update_window(window, |_, window, _| window.remove_window())
                .expect("close only the test window");
            let (journal, loaded) =
                AttachmentJournal::open(&fixture.journal()).expect("durable restart");
            let LoadedJournal::Present(restored) = loaded else {
                panic!("final snapshot exists");
            };
            assert!(!restored.attachments.contains_key(&removed));
            assert_eq!(restored.retired_blobs.len(), MAX_ENTRIES - 1);
            assert_eq!(
                journal
                    .read(&descriptors[1])
                    .expect("owned sibling remains")
                    .as_slice(),
                b"original"
            );
            assert_eq!(
                journal
                    .read(&descriptors[2])
                    .expect("unnamed catalog file is never swept")
                    .as_slice(),
                b"original"
            );
            let path = fixture
                .journal()
                .parent()
                .expect("profile")
                .join("attachment-spool")
                .join(format!("blob-{}.bin", descriptors[0].id().as_str()));
            assert!(
                !path.exists(),
                "only the named captured file was released after durable admission"
            );
        }

        #[gpui_kit::test]
        fn a_new_real_owner_settles_a_deferred_named_request_without_granting_release(
            cx: &mut TestAppContext,
        ) {
            let fixture = crate::recovery_tests::Fixture::new();
            let (mut journal, _) =
                AttachmentJournal::open(&fixture.journal()).expect("private original byte owner");
            let (snapshot, descriptors) = seed(&mut journal);
            let removed = AttachmentId::from(descriptors[0].id().as_str());
            let (owner, mut draft) = snapshot.drafts[0].clone();
            draft.content.files.push(DraftFile {
                id: removed.clone(),
                name: Some("removed.txt".into()),
                media_type: Some("text/plain".into()),
                byte_len: Some(8),
                source_document_id: None,
                content: DraftFileContent::Prepared,
            });
            let (window, workspace) = crate::workspace_tests::open(cx);
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.composer_key = None;
                    workspace.attachments = snapshot.attachments.clone();
                    workspace.retire_released_native_roots(vec![removed.clone()], window, cx);
                    assert!(workspace.has_pending_attachment_retirements());
                    workspace.drafts.save(owner, draft);
                    workspace
                        .validate_queue_intent_admission()
                        .expect("the replacement is a complete real draft owner");
                    workspace.retry_pending_attachment_retirements(window, cx);
                    assert!(
                        !workspace.has_pending_attachment_retirements(),
                        "the live owner now protects the named catalog bytes"
                    );
                    assert!(
                        workspace.attachments.contains_key(&removed)
                            && workspace.attachment_has_owner(&removed)
                    );
                    assert!(
                        workspace.retired_attachment_blobs.is_empty()
                            && !workspace.attachment_retirement.busy(descriptors[0].id())
                    );
                })
            })
            .expect("owner reconciliation cannot authorize a release");
            assert_eq!(
                journal
                    .read(&descriptors[0])
                    .expect("original held bytes remain unchanged")
                    .as_slice(),
                b"original"
            );
            cx.update_window(window, |_, window, _| window.remove_window())
                .expect("dispose only the test window");
        }

        #[gpui_kit::test]
        async fn failed_retirement_save_keeps_the_named_obligation_and_original_durable_bytes(
            cx: &mut TestAppContext,
        ) {
            cx.executor().allow_parking();
            let fixture = crate::recovery_tests::Fixture::new();
            let (mut journal, _) =
                AttachmentJournal::open(&fixture.journal()).expect("private journal");
            let (_, descriptors) = seed(&mut journal);
            drop(journal);
            let removed = AttachmentId::from(descriptors[0].id().as_str());
            let shared = AttachmentId::from(descriptors[1].id().as_str());
            let (window, workspace) = crate::workspace_tests::open(cx);
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.composer_key = None;
                    workspace.open_persistence(fixture.journal(), window, cx);
                })
            })
            .expect("hydrate the real original byte catalog");
            cx.condition(&workspace, |workspace, _| workspace.persistence_ready())
                .await;
            let finish = workspace.update(cx, |workspace, _| {
                workspace
                    .attachment_worker()
                    .expect("actual worker")
                    .finish()
            });
            assert_eq!(
                finish.await.expect("controlled worker shutdown"),
                Ok(CommitOutcome::Durable)
            );
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.retire_released_native_roots(
                        vec![removed.clone(), shared.clone()],
                        window,
                        cx,
                    );
                })
            })
            .expect("the next real journal save will be refused by its closed worker");
            cx.condition(&workspace, |workspace, _| {
                !workspace.attachment_retirement.busy(descriptors[0].id())
            })
            .await;
            workspace.update(cx, |workspace, _| {
                assert!(
                    workspace.has_pending_attachment_retirements(),
                    "a failed save cannot complete the named release obligation"
                );
                assert!(workspace.attachment_retirement.pending.contains(&removed));
                assert!(!workspace.attachment_retirement.pending.contains(&shared));
                assert!(
                    workspace
                        .retired_attachment_blobs
                        .contains(descriptors[0].id())
                );
                assert!(workspace.attachments.contains_key(&shared));
                assert!(
                    !workspace.close_in_progress(),
                    "this is a real storage refusal, not a close gate"
                );
            });
            let (journal, loaded) = AttachmentJournal::open(&fixture.journal())
                .expect("saved state remains readable after refusal");
            let LoadedJournal::Present(restored) = loaded else {
                panic!("original committed snapshot remains");
            };
            assert!(
                restored.attachments.contains_key(&removed),
                "the unwritten removal never replaces the original durable catalog"
            );
            assert!(!restored.retired_blobs.contains(descriptors[0].id()));
            assert_eq!(
                journal
                    .read(&descriptors[0])
                    .expect("no release before a durable intent")
                    .as_slice(),
                b"original"
            );
            assert_eq!(
                journal
                    .read(&descriptors[1])
                    .expect("independent sibling remains")
                    .as_slice(),
                b"original"
            );
            assert_eq!(
                sha256(
                    journal
                        .read(&descriptors[2])
                        .expect("unnamed bytes remain")
                        .as_slice()
                ),
                *descriptors[2].sha256()
            );
            cx.update_window(window, |_, window, _| window.remove_window())
                .expect("dispose only the test window");
        }
    }
}
