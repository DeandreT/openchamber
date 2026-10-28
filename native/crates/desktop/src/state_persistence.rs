//! Hydration, save barriers and normal-close draining for private native state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::DraftStore;

use crate::journal_worker::{
    CommitOutcome, JournalFailure, JournalReceipt, JournalResult, JournalWorker,
};
use crate::mutation_journal::{JournalSnapshot, LoadedJournal};
use crate::recovery_view::StorageNotice;
use crate::send_attempt::SendAttempts;
use crate::workspace::Workspace;

const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(crate) enum StorageIssue {
    Failure(JournalFailure),
    WriteRejected,
    Uncertain,
}

#[derive(Default)]
enum Status {
    #[default]
    Disabled,
    Loading,
    Ready,
    Fault {
        loaded: bool,
        issue: StorageIssue,
    },
    Closing,
}

#[derive(Default)]
pub(crate) struct StatePersistence {
    status: Status,
    worker: Option<JournalWorker>,
    path: Option<PathBuf>,
    revision: u64,
    epoch: u64,
    last_result: u64,
    opening: Option<Task<()>>,
    debounce: Option<Task<()>>,
    saving: Option<Task<()>>,
    closing: Option<Task<()>>,
}

#[derive(Clone, Copy)]
pub(crate) struct WriteStamp {
    epoch: u64,
    revision: u64,
}

impl StatePersistence {
    fn loaded(&self) -> bool {
        matches!(
            self.status,
            Status::Ready | Status::Closing | Status::Fault { loaded: true, .. }
        )
    }
}

impl Workspace {
    pub(crate) fn persistence_ready(&self) -> bool {
        matches!(self.persistence.status, Status::Ready)
    }

    pub(crate) fn persistence_closing(&self) -> bool {
        matches!(self.persistence.status, Status::Closing)
    }

    pub(crate) fn composer_editable(&self) -> bool {
        (self.is_synthetic() || self.persistence.loaded())
            && !matches!(self.persistence.status, Status::Closing | Status::Loading)
            && !self.selected.as_ref().is_some_and(|session| {
                self.send_attempts
                    .attempt(&self.mutation_key(session))
                    .is_some_and(|attempt| {
                        attempt.admission == crate::send_attempt::Admission::Preparing
                    })
            })
    }

    pub(crate) fn persistence_notice(&self) -> Option<StorageNotice> {
        match &self.persistence.status {
            Status::Disabled | Status::Ready => None,
            Status::Closing => Some(StorageNotice::Closing),
            Status::Loading => Some(StorageNotice::Loading),
            Status::Fault {
                issue: StorageIssue::WriteRejected,
                ..
            } => Some(StorageNotice::WriteRejected),
            Status::Fault {
                issue: StorageIssue::Uncertain,
                ..
            } => Some(StorageNotice::SyncUncertain),
            Status::Fault {
                issue: StorageIssue::Failure(error),
                ..
            } => Some(match error {
                JournalFailure::Locked => StorageNotice::Locked,
                JournalFailure::Invalid(_) | JournalFailure::UnsupportedVersion(_) => {
                    StorageNotice::Invalid
                }
                _ => StorageNotice::Io,
            }),
        }
    }

    pub(crate) fn start_persistence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_synthetic() {
            return;
        }
        let Some(path) =
            crate::install_id::config_directory().map(|path| path.join("drafts-and-sends.json"))
        else {
            self.persistence.status = Status::Fault {
                loaded: false,
                issue: StorageIssue::Failure(JournalFailure::Invalid(
                    "native profile is unavailable",
                )),
            };
            return;
        };
        self.open_persistence(path, window, cx);
    }

    pub(crate) fn open_persistence(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Once hydrated, memory may be newer than disk. Reopening cannot prove
        // that another process did not write while our lock was released.
        if self.persistence.loaded() {
            self.persistence.status = Status::Fault {
                loaded: true,
                issue: StorageIssue::Failure(JournalFailure::WorkerStopped),
            };
            cx.notify();
            return;
        }
        self.persistence.worker = None;
        self.persistence.path = Some(path.clone());
        self.persistence.status = Status::Loading;
        self.persistence.epoch = self.persistence.epoch.wrapping_add(1);
        let epoch = self.persistence.epoch;
        let (worker, ready) = JournalWorker::start(path);
        self.persistence.worker = Some(worker);
        self.persistence.opening = Some(cx.spawn_in(window, async move |this, cx| {
            let loaded = ready.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.persistence.epoch != epoch {
                    return;
                }
                match loaded {
                    Ok(loaded) => {
                        let snapshot = match loaded {
                            LoadedJournal::Missing => JournalSnapshot::default(),
                            LoadedJournal::Present(snapshot) => snapshot,
                        };
                        let attempts = match SendAttempts::from_snapshot(snapshot.attempts) {
                            Ok(attempts) => attempts,
                            Err(reason) => {
                                this.persistence.worker = None;
                                this.persistence.status = Status::Fault {
                                    loaded: false,
                                    issue: StorageIssue::Failure(JournalFailure::Invalid(reason)),
                                };
                                cx.notify();
                                return;
                            }
                        };
                        let mut drafts = DraftStore::new();
                        for (key, draft) in snapshot.drafts {
                            drafts.save(key, draft);
                        }
                        this.drafts = drafts;
                        this.send_attempts = attempts;
                        this.creations =
                            crate::create_attempt::CreateAttempts::restore(snapshot.creations);
                        this.restore_draft_previews();
                        this.restore_composer_draft(window, cx);
                        this.persistence.status = Status::Ready;
                    }
                    Err(error) => {
                        this.persistence.worker = None;
                        this.persistence.status = Status::Fault {
                            loaded: false,
                            issue: StorageIssue::Failure(error),
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Full snapshots are collected only at a save boundary, not on each keystroke.
    pub(crate) fn persistence_barrier(
        &mut self,
        cx: &gpui_kit::App,
    ) -> Option<(WriteStamp, JournalReceipt)> {
        if !self.persistence.loaded()
            || matches!(self.persistence.status, Status::Closing | Status::Loading)
        {
            return None;
        }
        self.save_composer_draft(cx);
        // During the durable barrier the field still displays the submitted
        // text. The attempt owns that copy in storage; a later identical draft
        // typed after dispatch is a separate value and remains in the journal.
        let preparing: HashMap<_, _> = self
            .send_attempts
            .iter()
            .filter(|(_, attempt)| {
                attempt.admission == crate::send_attempt::Admission::Preparing
                    && attempt.origin == crate::send_attempt::SendOrigin::Existing
            })
            .map(|(_, attempt)| (&attempt.owner, attempt.draft.text.as_str()))
            .collect();
        let snapshot = JournalSnapshot {
            drafts: self
                .drafts
                .iter()
                .filter(|(key, draft)| preparing.get(key).is_none_or(|text| *text != draft.text))
                .map(|(key, draft)| (key.clone(), draft.clone()))
                .collect(),
            attempts: self.send_attempts.snapshot(),
            creations: self.creations.snapshot(),
        };
        let worker = self.persistence.worker.as_ref()?;
        self.persistence.revision += 1;
        Some((
            WriteStamp {
                epoch: self.persistence.epoch,
                revision: self.persistence.revision,
            },
            worker.submit(snapshot),
        ))
    }

    pub(crate) fn record_persistence(
        &mut self,
        stamp: WriteStamp,
        result: &JournalResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let durable = matches!(result, Ok(CommitOutcome::Durable));
        if stamp.epoch != self.persistence.epoch {
            return false;
        }
        if stamp.revision >= self.persistence.last_result
            && !matches!(self.persistence.status, Status::Closing)
        {
            self.persistence.last_result = stamp.revision;
            self.persistence.status = match result {
                Ok(CommitOutcome::Durable) => Status::Ready,
                Ok(CommitOutcome::DirectorySyncUncertain(_)) => Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Uncertain,
                },
                Err(JournalFailure::Invalid(_)) => Status::Fault {
                    loaded: true,
                    issue: StorageIssue::WriteRejected,
                },
                Err(error) => Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Failure(error.clone()),
                },
            };
            cx.notify();
        }
        durable && !matches!(self.persistence.status, Status::Closing)
    }

    pub(crate) fn queue_persistence(&mut self, cx: &mut Context<Self>) {
        if !self.persistence.loaded()
            || matches!(self.persistence.status, Status::Closing | Status::Loading)
        {
            return;
        }
        self.persistence.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| this.start_background_save(cx));
        }));
    }

    pub(crate) fn start_background_save(&mut self, cx: &mut Context<Self>) {
        let Some((revision, saved)) = self.persistence_barrier(cx) else {
            return;
        };
        self.persistence.saving = Some(cx.spawn(async move |this, cx| {
            let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let _ = this.update(cx, |this, cx| {
                this.record_persistence(revision, &result, cx);
            });
        }));
    }

    pub(crate) fn retry_persistence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.persistence.loaded() && self.persistence.worker.is_some() {
            self.start_background_save(cx);
        } else if let Some(path) = self.persistence.path.clone() {
            self.open_persistence(path, window, cx);
        } else {
            self.start_persistence(window, cx);
        }
    }

    pub(crate) fn flush_before_quit(&mut self, cx: &gpui_kit::App) -> Option<JournalReceipt> {
        self.persistence.debounce = None;
        let (_, saved) = self.persistence_barrier(cx)?;
        // The flush receipt covers this save and every earlier accepted write.
        drop(saved);
        self.persistence.worker.as_ref().map(JournalWorker::flush)
    }

    /// Close controls wait for the last full snapshot and release the file lock.
    pub(crate) fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_synthetic() || !self.persistence.loaded() {
            window.remove_window();
            return;
        }
        if matches!(self.persistence.status, Status::Closing) {
            return;
        }
        self.persistence.debounce = None;
        let Some((stamp, saved)) = self.persistence_barrier(cx) else {
            return;
        };
        self.persistence.status = Status::Closing;
        self.persistence.closing = Some(cx.spawn_in(window, async move |this, cx| {
            let saved = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let finished = this.update_in(cx, |this, _, cx| {
                if this.persistence.epoch != stamp.epoch {
                    return None;
                }
                // Older background receipts must not clear a failed close.
                this.persistence.last_result = stamp.revision;
                if matches!(saved, Ok(CommitOutcome::Durable)) {
                    return this.persistence.worker.as_ref().map(JournalWorker::finish);
                }
                // Keep the worker and its exclusive lock for an explicit retry.
                // A stopped worker keeps rejecting writes; it is never reopened
                // over possibly newer data from another process.
                this.persistence.status = Status::Ready;
                this.record_persistence(stamp, &saved, cx);
                None
            });
            let Ok(Some(finished)) = finished else {
                return;
            };
            let finished = finished.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.persistence.epoch != stamp.epoch {
                    return;
                }
                if matches!(finished, Ok(CommitOutcome::Durable)) {
                    this.persistence.worker = None;
                    window.remove_window();
                } else {
                    let issue = match finished {
                        Err(JournalFailure::Invalid(_)) => StorageIssue::WriteRejected,
                        Err(error) => StorageIssue::Failure(error),
                        _ => StorageIssue::Uncertain,
                    };
                    this.persistence.status = Status::Fault {
                        loaded: true,
                        issue,
                    };
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, TestAppContext};

    #[gpui_kit::test]
    fn obsolete_writer_receipts_cannot_clear_current_failure(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.epoch = 2;
                workspace.persistence.status = Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Failure(JournalFailure::Locked),
                };
                assert!(!workspace.record_persistence(
                    WriteStamp {
                        epoch: 1,
                        revision: 5
                    },
                    &Ok(CommitOutcome::Durable),
                    cx
                ));
                assert_eq!(workspace.persistence_notice(), Some(StorageNotice::Locked));
            })
        });
    }

    #[gpui_kit::test]
    fn older_receipts_cannot_clear_failed_close(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Closing;
                workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 1,
                    },
                    &Ok(CommitOutcome::Durable),
                    cx,
                );
                assert!(workspace.persistence_closing());
                workspace.persistence.status = Status::Ready;
                workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 2,
                    },
                    &Err(JournalFailure::Io(std::io::ErrorKind::Other)),
                    cx,
                );
                workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 1,
                    },
                    &Ok(CommitOutcome::Durable),
                    cx,
                );
                assert_eq!(workspace.persistence_notice(), Some(StorageNotice::Io));
            });
        });
    }

    #[gpui_kit::test]
    fn hydrated_memory_cannot_reopen_a_released_writer(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Failure(JournalFailure::WorkerStopped),
                };
                workspace.persistence.path = Some(PathBuf::from("unused-journal.json"));
                workspace.retry_persistence(window, cx);
                assert!(workspace.persistence.worker.is_none());
                assert!(matches!(
                    workspace.persistence.status,
                    Status::Fault {
                        loaded: true,
                        issue: StorageIssue::Failure(JournalFailure::WorkerStopped),
                    }
                ));
            });
        })
        .expect("window open");
    }

    #[gpui_kit::test]
    fn rejected_writes_are_distinct_from_unreadable_saved_data(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Ready;
                assert!(!workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 1
                    },
                    &Err(JournalFailure::Invalid("text size limit exceeded")),
                    cx
                ));
                assert_eq!(
                    workspace.persistence_notice(),
                    Some(StorageNotice::WriteRejected)
                );
            })
        });
    }
}
