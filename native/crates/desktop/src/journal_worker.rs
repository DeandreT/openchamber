//! Serialized journal I/O on a dedicated thread. No method performs file I/O
//! or waits for queue capacity on the calling thread.
//!
//! Submit full snapshots in mutation order after hydration succeeds. There is
//! one in-flight snapshot, one replaceable pending snapshot, and at most 128
//! outstanding save/flush barriers. Superseded pending snapshots keep their
//! barriers: the newest snapshot's durable commit satisfies them all. Callers
//! must check that a send is still current after awaiting its barrier.
//!
//! Attachment jobs share this thread and lock. Pending full snapshots take
//! priority, so a coalesced removal can cancel an obsolete blob request at its
//! committed-authority check. No pending intent grants file access. Attachment
//! errors affect only their own receipt and never replace the journal outcome.
//! At most 32 active jobs, queued jobs and unread read replies combined are
//! admitted within 40 MiB of buffers. This includes each put's 16 KiB comparison
//! workspace and each read's oversize sentinel byte. Each payload is at most 20 MiB.
//! Consumer edit jobs also charge their captured binding's retained metadata.
//! Completed unread read replies retain their byte and slot reservations. Once
//! consumed, returned BlobBytes belong to the caller and leave this budget.
//! Flush and finish wait for I/O completion, independently of receipt consumption.
//!
//! Failure clears queued work and fails its barriers. A subsequent submit is
//! an explicit retry. Flush reports the latest accepted work's outcome; finish
//! seals admission, drains accepted work and releases the file lock before its
//! receipt. Dropping the sole handle also drains accepted work without joining.
//! A canceled receipt means the worker stopped without confirming durability.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::{Context, Poll};
use std::thread;

use tokio::sync::oneshot;

use openchamber_native_core::draft::DraftKey;
use openchamber_native_core::ids::AttachmentId;
use openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES;
use openchamber_native_transport::queue_manifest::QueuePayloadFilesProof;

use crate::attachment_catalog::PreparationHold;
use crate::attachment_io::{
    AttachmentAuthority, AttachmentFailure, AttachmentJournal, QueueAcknowledgementPermit,
    QueueConsumerEditStoredChunk, QueueConsumerEditVerifiedCapture, QueueVerifiedCapture,
};
use crate::attachment_spool::{
    BlobBytes, BlobDescriptor, BlobId, BlobReceipt as SpoolBlobReceipt, PUT_COMPARE_BYTES,
    SpoolCommit, SpoolError,
};
use crate::mutation_journal::{JournalCommit, JournalError, JournalSnapshot, LoadedJournal};
use crate::queue_capture::{
    QUEUE_CHUNK_BYTES, QueueChunkHold, QueueOperationId, QueueVerification,
};
use crate::queue_storage::QueueConsumerEditBinding;

const MAX_WAITERS: usize = 128;
const MAX_BLOB_JOBS: usize = 32;
const MAX_BLOB_BYTES: usize = 2 * MAX_PREPARED_ATTACHMENT_BYTES;

/// Contains only classifications, never paths, draft text or provider data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JournalFailure {
    Io(io::ErrorKind),
    Locked,
    Invalid(&'static str),
    UnsupportedVersion(u32),
    NotHydrated,
    AtCapacity,
    Closed,
    WorkerStopped,
}

impl From<JournalError> for JournalFailure {
    fn from(error: JournalError) -> Self {
        match error {
            JournalError::Io(error) => Self::Io(error.kind()),
            JournalError::Locked => Self::Locked,
            JournalError::Invalid(reason) => Self::Invalid(reason),
            JournalError::UnsupportedVersion(version) => Self::UnsupportedVersion(version),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    Durable,
    DirectorySyncUncertain(io::ErrorKind),
}

pub(crate) type JournalResult = Result<CommitOutcome, JournalFailure>;
pub(crate) type JournalReceipt = oneshot::Receiver<JournalResult>;
type Waiter = oneshot::Sender<JournalResult>;
pub(crate) type HydrationReceipt = oneshot::Receiver<Result<LoadedJournal, JournalFailure>>;

/// Classifications only. Spool errors never carry file paths or captured bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlobFailure {
    Journal(JournalFailure),
    Attachment(AttachmentFailure),
}

pub(crate) type BlobReceipt = oneshot::Receiver<Result<SpoolBlobReceipt, BlobFailure>>;
pub(crate) type BlobReleaseReceipt = oneshot::Receiver<Result<SpoolCommit, BlobFailure>>;
pub(crate) type QueueVerificationReceipt =
    oneshot::Receiver<Result<QueueVerifiedCapture, BlobFailure>>;
pub(crate) type QueueConsumerEditChunkReceipt =
    oneshot::Receiver<Result<QueueConsumerEditStoredChunk, BlobFailure>>;
pub(crate) type QueueConsumerEditVerificationReceipt =
    oneshot::Receiver<Result<QueueConsumerEditVerifiedCapture, BlobFailure>>;
pub(crate) type QueueAcknowledgementReceipt =
    oneshot::Receiver<Result<QueueAcknowledgementPermit, BlobFailure>>;

#[cfg(test)]
type ReplyGateHandle<T> = Arc<Mutex<ReplyGateState<T>>>;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyGateDisposition {
    Holding,
    Released,
    Cancelled,
}

#[cfg(test)]
struct ReplyGateState<T> {
    reached: bool,
    signal: Option<oneshot::Sender<()>>,
    held: Option<(oneshot::Sender<T>, T)>,
    disposition: ReplyGateDisposition,
}

/// Holds only a computed reply. The worker and its journal lock keep progressing.
#[cfg(test)]
pub(crate) struct ReplyGate<T> {
    state: ReplyGateHandle<T>,
    signal: oneshot::Receiver<()>,
}

#[cfg(test)]
impl<T> ReplyGate<T> {
    fn new() -> Self {
        let (signal, receipt) = oneshot::channel();
        Self {
            state: Arc::new(Mutex::new(ReplyGateState {
                reached: false,
                signal: Some(signal),
                held: None,
                disposition: ReplyGateDisposition::Holding,
            })),
            signal: receipt,
        }
    }

    pub(crate) async fn reached(&mut self) {
        (&mut self.signal)
            .await
            .expect("the armed real reply reaches its test gate");
    }

    pub(crate) fn has_reached(&self) -> bool {
        self.state
            .lock()
            .expect("reply gate is not poisoned")
            .reached
    }

    pub(crate) fn release(&self) {
        let held = {
            let mut state = self.state.lock().expect("reply gate is not poisoned");
            if state.disposition == ReplyGateDisposition::Holding {
                state.disposition = ReplyGateDisposition::Released;
            }
            state.held.take()
        };
        if let Some((reply, result)) = held {
            let _ = reply.send(result);
        }
    }

    pub(crate) fn cancel(&self) {
        let held = {
            let mut state = self.state.lock().expect("reply gate is not poisoned");
            state.disposition = ReplyGateDisposition::Cancelled;
            state.held.take()
        };
        drop(held);
    }

    fn hold(state: &ReplyGateHandle<T>, reply: oneshot::Sender<T>, result: T) {
        let (signal, deliver) = {
            let mut state = state.lock().expect("reply gate is not poisoned");
            assert!(
                !state.reached,
                "a reply gate captures exactly one computed result"
            );
            state.reached = true;
            let signal = state.signal.take();
            let deliver = if state.disposition == ReplyGateDisposition::Holding {
                state.held = Some((reply, result));
                None
            } else {
                Some((state.disposition, reply, result))
            };
            (signal, deliver)
        };
        if let Some(signal) = signal {
            let _ = signal.send(());
        }
        if let Some((ReplyGateDisposition::Released, reply, result)) = deliver {
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
impl<T> Drop for ReplyGate<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A read reply retains its buffer and slot budget until consumed or dropped.
/// The job separately retains the reservation until I/O and its payload cleanup
/// finish, so dropping this receipt early never admits overlapping active buffers.
/// A successful result transfers BlobBytes to the caller outside this budget.
pub(crate) struct BlobReadReceipt {
    receiver: Option<oneshot::Receiver<Result<BlobBytes, BlobFailure>>>,
    reservation: Option<Arc<BlobReservation>>,
}

impl BlobReadReceipt {
    #[cfg(test)]
    pub(crate) fn blocking_recv(
        mut self,
    ) -> Result<Result<BlobBytes, BlobFailure>, oneshot::error::RecvError> {
        let result = self
            .receiver
            .take()
            .expect("unconsumed read receipt")
            .blocking_recv();
        drop(self.reservation.take());
        result
    }

    #[cfg(test)]
    pub(crate) fn try_recv(
        &mut self,
    ) -> Result<Result<BlobBytes, BlobFailure>, oneshot::error::TryRecvError> {
        let result = match &mut self.receiver {
            Some(receiver) => receiver.try_recv(),
            None => Err(oneshot::error::TryRecvError::Closed),
        };
        if !matches!(&result, Err(oneshot::error::TryRecvError::Empty)) {
            self.finish_receipt();
        }
        result
    }

    fn finish_receipt(&mut self) {
        // Dispose of a buffered reply before freeing its reservation on Drop.
        drop(self.receiver.take());
        drop(self.reservation.take());
    }
}

impl Future for BlobReadReceipt {
    type Output = Result<Result<BlobBytes, BlobFailure>, oneshot::error::RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let receipt = self.get_mut();
        let receiver = receipt.receiver.as_mut().expect("unconsumed read receipt");
        match Pin::new(receiver).poll(cx) {
            Poll::Ready(result) => {
                receipt.finish_receipt();
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for BlobReadReceipt {
    fn drop(&mut self) {
        self.finish_receipt();
    }
}

struct BlobReservation {
    shared: Weak<Shared>,
    bytes: usize,
}

impl Drop for BlobReservation {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        state.blob_bytes -= self.bytes;
        state.blob_reservations -= 1;
    }
}

struct ReservedBlobJob {
    job: BlobJob,
    // A weak back-reference in the reservation avoids a queue/Shared cycle.
    reservation: Arc<BlobReservation>,
}

enum BlobJob {
    QueuePut {
        hold: QueueChunkHold,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<SpoolBlobReceipt, BlobFailure>>,
    },
    QueueResume {
        hold: QueueChunkHold,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<SpoolBlobReceipt, BlobFailure>>,
    },
    QueueRead {
        descriptor: BlobDescriptor,
        reply: oneshot::Sender<Result<BlobBytes, BlobFailure>>,
    },
    QueueVerify {
        verification: QueueVerification,
        reply: oneshot::Sender<Result<QueueVerifiedCapture, BlobFailure>>,
    },
    ConsumerEditPut {
        binding: QueueConsumerEditBinding,
        hold: QueueChunkHold,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<QueueConsumerEditStoredChunk, BlobFailure>>,
    },
    ConsumerEditResume {
        binding: QueueConsumerEditBinding,
        hold: QueueChunkHold,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<QueueConsumerEditStoredChunk, BlobFailure>>,
    },
    ConsumerEditVerify {
        binding: QueueConsumerEditBinding,
        verification: QueueVerification,
        reply: oneshot::Sender<Result<QueueConsumerEditVerifiedCapture, BlobFailure>>,
    },
    QueueAcknowledge {
        operation: QueueOperationId,
        proofs: Vec<QueuePayloadFilesProof>,
        reply: oneshot::Sender<Result<QueueAcknowledgementPermit, BlobFailure>>,
    },
    QueueRelease {
        id: BlobId,
        reply: oneshot::Sender<Result<SpoolCommit, BlobFailure>>,
    },
    Put {
        hold: PreparationHold,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<SpoolBlobReceipt, BlobFailure>>,
    },
    AttachmentResume {
        hold: PreparationHold,
        descriptor: BlobDescriptor,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<SpoolBlobReceipt, BlobFailure>>,
    },
    DownloadPut {
        hold: PreparationHold,
        descriptor: BlobDescriptor,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<SpoolBlobReceipt, BlobFailure>>,
    },
    Read {
        descriptor: BlobDescriptor,
        reply: oneshot::Sender<Result<BlobBytes, BlobFailure>>,
    },
    Release {
        id: BlobId,
        reply: oneshot::Sender<Result<SpoolCommit, BlobFailure>>,
    },
}

impl BlobJob {
    fn check_authority(
        &self,
        authority: Option<&AttachmentAuthority>,
    ) -> Result<(), AttachmentFailure> {
        let authority = authority.ok_or(AttachmentFailure::NoDurableAuthority)?;
        match self {
            Self::QueuePut { hold, .. } => authority.check_queue_put(hold),
            Self::QueueResume { hold, .. } => authority.check_queue_resume(hold),
            Self::QueueRead { descriptor, .. } => authority.check_queue_read(descriptor),
            Self::QueueVerify { verification, .. } => {
                authority.check_queue_verification(verification)
            }
            Self::ConsumerEditPut { binding, hold, .. } => {
                authority.check_consumer_edit_put(binding, hold)
            }
            Self::ConsumerEditResume { binding, hold, .. } => {
                authority.check_consumer_edit_resume(binding, hold)
            }
            Self::ConsumerEditVerify {
                binding,
                verification,
                ..
            } => authority.check_consumer_edit_verification(binding, verification),
            Self::QueueAcknowledge { operation, .. } => {
                authority.queue_acknowledgement(operation).map(|_| ())
            }
            Self::QueueRelease { id, .. } => authority.check_queue_release(id),
            Self::Put { hold, .. } => authority.check_put(hold),
            Self::AttachmentResume {
                hold, descriptor, ..
            } => authority.check_attachment_resume(hold, descriptor),
            Self::DownloadPut {
                hold, descriptor, ..
            } => authority.check_attachment_resume(hold, descriptor),
            Self::Read { descriptor, .. } => authority.check_read(descriptor),
            Self::Release { id, .. } => authority.check_release(id),
        }
    }

    fn owned_bytes(&self) -> usize {
        match self {
            // Charge the allocation, including spare capacity, while we own it.
            Self::Put { bytes, .. }
            | Self::QueuePut { bytes, .. }
            | Self::QueueResume { bytes, .. }
            | Self::AttachmentResume { bytes, .. }
            | Self::DownloadPut { bytes, .. } => bytes.capacity().saturating_add(PUT_COMPARE_BYTES),
            Self::ConsumerEditPut { binding, bytes, .. }
            | Self::ConsumerEditResume { binding, bytes, .. } => bytes
                .capacity()
                .saturating_add(PUT_COMPARE_BYTES)
                .saturating_add(binding.retained_bytes()),
            // Read admission already limits the descriptor to 20 MiB. The spool
            // allocates this extra byte before reading a potentially changed file.
            Self::Read { descriptor, .. } | Self::QueueRead { descriptor, .. } => {
                descriptor.byte_len() as usize + 1
            }
            Self::QueueVerify { verification, .. } => verification
                .reference
                .chunks()
                .iter()
                .filter_map(|chunk| chunk.descriptor().ok())
                .map(|descriptor| descriptor.byte_len() as usize + 1)
                .max()
                .unwrap_or(0),
            Self::ConsumerEditVerify {
                binding,
                verification,
                ..
            } => verification
                .reference
                .chunks()
                .iter()
                .filter_map(|chunk| chunk.descriptor().ok())
                .map(|descriptor| descriptor.byte_len() as usize + 1)
                .max()
                .unwrap_or(0)
                .saturating_add(binding.retained_bytes()),
            Self::QueueAcknowledge { proofs, .. } => (MAX_PREPARED_ATTACHMENT_BYTES
                .max(QUEUE_CHUNK_BYTES)
                + 1)
            .saturating_add(proofs.capacity() * std::mem::size_of::<QueuePayloadFilesProof>())
            .saturating_add(
                proofs
                    .iter()
                    .map(QueuePayloadFilesProof::retained_bytes)
                    .sum::<usize>(),
            ),
            Self::Release { .. } | Self::QueueRelease { .. } => 0,
        }
    }

    fn execute(self, journal: &mut AttachmentJournal) {
        match self {
            Self::QueuePut { hold, bytes, reply } => {
                let _ = reply.send(
                    journal
                        .put_queue(&hold, &bytes)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::QueueResume { hold, bytes, reply } => {
                let _ = reply.send(
                    journal
                        .resume_queue(&hold, &bytes)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::QueueRead { descriptor, reply } => {
                let _ = reply.send(
                    journal
                        .read_queue(&descriptor)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::QueueVerify {
                verification,
                reply,
            } => {
                let _ = reply.send(
                    journal
                        .verify_queue(&verification)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::ConsumerEditPut {
                binding,
                hold,
                bytes,
                reply,
            } => {
                let result = journal
                    .put_consumer_edit_chunk(binding, hold, &bytes)
                    .map_err(BlobFailure::Attachment);
                let _ = reply.send(result);
            }
            Self::ConsumerEditResume {
                binding,
                hold,
                bytes,
                reply,
            } => {
                let result = journal
                    .resume_consumer_edit_chunk(binding, hold, &bytes)
                    .map_err(BlobFailure::Attachment);
                let _ = reply.send(result);
            }
            Self::ConsumerEditVerify {
                binding,
                verification,
                reply,
            } => {
                let result = journal
                    .verify_consumer_edit_capture(binding, verification)
                    .map_err(BlobFailure::Attachment);
                let _ = reply.send(result);
            }
            Self::QueueAcknowledge {
                operation,
                proofs,
                reply,
            } => {
                let _ = reply.send(
                    journal
                        .authorize_queue_acknowledgement(&operation, &proofs)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::QueueRelease { id, reply } => {
                let _ = reply.send(journal.release_queue(&id).map_err(BlobFailure::Attachment));
            }
            Self::Put { hold, bytes, reply } => {
                let result = journal.put(&hold, &bytes).map_err(BlobFailure::Attachment);
                let _ = reply.send(result);
            }
            Self::AttachmentResume {
                hold,
                descriptor,
                bytes,
                reply,
            } => {
                let _ = reply.send(
                    journal
                        .resume_attachment(&hold, &descriptor, &bytes)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::DownloadPut {
                hold,
                descriptor,
                bytes,
                reply,
            } => {
                let _ = reply.send(
                    journal
                        .put_downloaded_attachment(&hold, &descriptor, &bytes)
                        .map_err(BlobFailure::Attachment),
                );
            }
            Self::Read { descriptor, reply } => {
                let result = journal.read(&descriptor).map_err(BlobFailure::Attachment);
                let _ = reply.send(result);
            }
            Self::Release { id, reply } => {
                let result = journal.release(&id).map_err(BlobFailure::Attachment);
                let _ = reply.send(result);
            }
        }
    }

    fn fail(self, error: BlobFailure) {
        match self {
            Self::Put { reply, .. }
            | Self::QueuePut { reply, .. }
            | Self::QueueResume { reply, .. }
            | Self::AttachmentResume { reply, .. }
            | Self::DownloadPut { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Read { reply, .. } | Self::QueueRead { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Release { reply, .. } | Self::QueueRelease { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::QueueVerify { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::ConsumerEditPut { reply, .. } | Self::ConsumerEditResume { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::ConsumerEditVerify { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::QueueAcknowledge { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

struct Barrier {
    reply: Waiter,
    blobs_through: u64,
    #[cfg(test)]
    reply_gate: Option<ReplyGateHandle<JournalResult>>,
}

impl Barrier {
    fn complete(self, outcome: JournalResult) {
        #[cfg(test)]
        if let Some(gate) = self.reply_gate {
            ReplyGate::hold(&gate, self.reply, outcome);
            return;
        }
        let _ = self.reply.send(outcome);
    }
}

struct ResolvedBarrier {
    barrier: Barrier,
    outcome: JournalResult,
}

enum Phase {
    Opening,
    Ready,
    Failed(JournalFailure),
}

struct Pending {
    snapshot: JournalSnapshot,
    waiters: Vec<Barrier>,
}

struct State {
    phase: Phase,
    hydration: Option<oneshot::Sender<Result<LoadedJournal, JournalFailure>>>,
    pending: Option<Pending>,
    active: Vec<Barrier>,
    drains: Vec<ResolvedBarrier>,
    blobs: VecDeque<ReservedBlobJob>,
    blob_bytes: usize,
    blob_reservations: usize,
    accepted_blobs: u64,
    completed_blobs: u64,
    // Admission-only copy of the last published durable ownership. Execution
    // rechecks the worker-owned AttachmentJournal, so later removals dominate.
    // This copy prevents a future pending intent from admitting an early job.
    authority: Option<AttachmentAuthority>,
    writing: bool,
    closing: bool,
    stopped: bool,
    finish: Option<Waiter>,
    last: JournalResult,
    #[cfg(test)]
    verification_reply_gate: Option<(
        QueueVerification,
        ReplyGateHandle<Result<QueueVerifiedCapture, BlobFailure>>,
    )>,
    #[cfg(test)]
    consumer_edit_verification_reply_gate: Option<ConsumerEditVerificationReplyGate>,
    #[cfg(test)]
    finish_reply_gate: Option<ReplyGateHandle<JournalResult>>,
    #[cfg(test)]
    snapshot_reply_gate: Option<ReplyGateHandle<JournalResult>>,
}

#[cfg(test)]
type ConsumerEditVerificationReplyGate = (
    QueueConsumerEditBinding,
    QueueVerification,
    ReplyGateHandle<Result<QueueConsumerEditVerifiedCapture, BlobFailure>>,
);

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// A single ownership handle. Dropping it requests graceful asynchronous stop.
pub(crate) struct JournalWorker {
    shared: Arc<Shared>,
}

impl JournalWorker {
    pub(crate) fn start(path: PathBuf) -> (Self, HydrationReceipt) {
        Self::start_with_io(
            move || AttachmentJournal::open(&path),
            AttachmentJournal::save,
            |journal, job| job.execute(journal),
        )
    }

    #[cfg(test)]
    fn start_with<W, O, S>(open: O, save: S) -> (Self, HydrationReceipt)
    where
        W: Send + 'static,
        O: FnOnce() -> Result<(W, LoadedJournal), JournalError> + Send + 'static,
        S: FnMut(&mut W, &JournalSnapshot) -> Result<JournalCommit, JournalError> + Send + 'static,
    {
        Self::start_with_io(open, save, |_, job| {
            job.fail(BlobFailure::Attachment(
                AttachmentFailure::NoDurableAuthority,
            ));
        })
    }

    fn start_with_io<W, O, S, B>(open: O, save: S, blobs: B) -> (Self, HydrationReceipt)
    where
        W: Send + 'static,
        O: FnOnce() -> Result<(W, LoadedJournal), JournalError> + Send + 'static,
        S: FnMut(&mut W, &JournalSnapshot) -> Result<JournalCommit, JournalError> + Send + 'static,
        B: FnMut(&mut W, BlobJob) + Send + 'static,
    {
        let (hydration, receipt) = oneshot::channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                phase: Phase::Opening,
                hydration: Some(hydration),
                pending: None,
                active: Vec::new(),
                drains: Vec::new(),
                blobs: VecDeque::new(),
                blob_bytes: 0,
                blob_reservations: 0,
                accepted_blobs: 0,
                completed_blobs: 0,
                authority: None,
                writing: false,
                closing: false,
                stopped: false,
                finish: None,
                last: Ok(CommitOutcome::Durable),
                #[cfg(test)]
                verification_reply_gate: None,
                #[cfg(test)]
                consumer_edit_verification_reply_gate: None,
                #[cfg(test)]
                finish_reply_gate: None,
                #[cfg(test)]
                snapshot_reply_gate: None,
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("native-journal".to_owned())
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run(&worker_shared, open, save, blobs);
                }));
                if result.is_err() {
                    stop_failed(&worker_shared, JournalFailure::WorkerStopped);
                }
            });
        if let Err(error) = spawned {
            stop_failed(&shared, JournalFailure::Io(error.kind()));
        }
        (Self { shared }, receipt)
    }

    #[cfg(test)]
    pub(crate) fn hold_queue_verification_reply_for_test(
        &self,
        verification: QueueVerification,
    ) -> ReplyGate<Result<QueueVerifiedCapture, BlobFailure>> {
        let gate = ReplyGate::new();
        let mut state = self
            .shared
            .state
            .lock()
            .expect("journal state is not poisoned");
        assert!(
            state.verification_reply_gate.is_none(),
            "only one exact verification reply gate is armed per worker"
        );
        state.verification_reply_gate = Some((verification, gate.state.clone()));
        gate
    }

    #[cfg(test)]
    pub(crate) fn hold_consumer_edit_verification_reply_for_test(
        &self,
        binding: QueueConsumerEditBinding,
        verification: QueueVerification,
    ) -> ReplyGate<Result<QueueConsumerEditVerifiedCapture, BlobFailure>> {
        let gate = ReplyGate::new();
        let mut state = self
            .shared
            .state
            .lock()
            .expect("journal state is not poisoned");
        assert!(
            state.consumer_edit_verification_reply_gate.is_none(),
            "only one exact consumer edit verification reply gate is armed per worker"
        );
        state.consumer_edit_verification_reply_gate =
            Some((binding, verification, gate.state.clone()));
        gate
    }

    /// Holds one accepted snapshot's computed reply without delaying disk I/O,
    /// later snapshots, flush checkpoints, or the real writer's finish.
    #[cfg(test)]
    pub(crate) fn hold_next_snapshot_reply_for_test(&self) -> ReplyGate<JournalResult> {
        let gate = ReplyGate::new();
        let mut state = self
            .shared
            .state
            .lock()
            .expect("journal state is not poisoned");
        assert!(
            state.snapshot_reply_gate.is_none()
                && matches!(state.phase, Phase::Ready)
                && !state.closing,
            "arm one next-snapshot reply gate on the live hydrated worker"
        );
        state.snapshot_reply_gate = Some(gate.state.clone());
        gate
    }

    #[cfg(test)]
    pub(crate) fn hold_finish_reply_for_test(&self) -> ReplyGate<JournalResult> {
        let gate = ReplyGate::new();
        let mut state = self
            .shared
            .state
            .lock()
            .expect("journal state is not poisoned");
        assert!(
            state.finish_reply_gate.is_none() && !state.closing,
            "arm the one finish reply gate before sealing admission"
        );
        state.finish_reply_gate = Some(gate.state.clone());
        gate
    }

    /// Rejects pre-hydration, closed and over-capacity submissions immediately.
    /// The rejected snapshot never replaces accepted pending work.
    pub(crate) fn submit(&self, snapshot: JournalSnapshot) -> JournalReceipt {
        let (waiter, receipt) = oneshot::channel();
        let Ok(mut state) = self.shared.state.lock() else {
            let _ = waiter.send(Err(JournalFailure::WorkerStopped));
            return receipt;
        };
        if let Some(error) = admission_error(&state) {
            let _ = waiter.send(Err(error));
            return receipt;
        }
        let waiter = Barrier {
            reply: waiter,
            blobs_through: 0,
            #[cfg(test)]
            reply_gate: state.snapshot_reply_gate.take(),
        };
        let replaced = if let Some(pending) = &mut state.pending {
            pending.waiters.push(waiter);
            Some(std::mem::replace(&mut pending.snapshot, snapshot))
        } else {
            state.pending = Some(Pending {
                snapshot,
                waiters: vec![waiter],
            });
            None
        };
        drop(state);
        self.shared.wake.notify_one();
        // Releasing replaced payload memory does not hold the queue mutex.
        drop(replaced);
        receipt
    }

    /// Waits for all work accepted before this call. Later snapshots can
    /// supersede that work and therefore delay this barrier until they commit.
    pub(crate) fn flush(&self) -> JournalReceipt {
        let (waiter, receipt) = oneshot::channel();
        let Ok(mut state) = self.shared.state.lock() else {
            let _ = waiter.send(Err(JournalFailure::WorkerStopped));
            return receipt;
        };
        if let Some(error) = admission_error(&state) {
            let _ = waiter.send(Err(error));
        } else {
            let barrier = Barrier {
                reply: waiter,
                blobs_through: state.accepted_blobs,
                #[cfg(test)]
                reply_gate: None,
            };
            if let Some(pending) = &mut state.pending {
                pending.waiters.push(barrier);
            } else if state.writing {
                state.active.push(barrier);
            } else {
                let outcome = state.last.clone();
                resolve(&mut state, vec![barrier], &outcome);
            }
        }
        receipt
    }

    /// Captures one pre-put intent. Await its durable journal save before calling.
    /// A callback must still check its captured owner, ID and generation.
    pub(crate) fn put_attachment(
        &self,
        owner: DraftKey,
        id: AttachmentId,
        generation: u64,
        blob: BlobId,
        bytes: Vec<u8>,
    ) -> BlobReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > MAX_PREPARED_ATTACHMENT_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::Put {
            hold: PreparationHold {
                owner,
                id,
                generation,
                blob,
            },
            bytes,
            reply,
        }));
        receipt
    }

    /// Explicitly resumes only a committed take preparation and its original descriptor.
    pub(crate) fn resume_attachment(
        &self,
        hold: PreparationHold,
        descriptor: BlobDescriptor,
        bytes: Vec<u8>,
    ) -> BlobReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > MAX_PREPARED_ATTACHMENT_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::AttachmentResume {
            hold,
            descriptor,
            bytes,
            reply,
        }));
        receipt
    }

    /// First put of a queue download under its exact committed binding. This
    /// entry never repairs an existing partial file; explicit Retry owns that.
    pub(crate) fn put_downloaded_attachment(
        &self,
        hold: PreparationHold,
        descriptor: BlobDescriptor,
        bytes: Vec<u8>,
    ) -> BlobReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > MAX_PREPARED_ATTACHMENT_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::DownloadPut {
            hold,
            descriptor,
            bytes,
            reply,
        }));
        receipt
    }

    /// Accepts the complete descriptor, never a blob identity and guessed length.
    pub(crate) fn read_attachment(&self, descriptor: BlobDescriptor) -> BlobReadReceipt {
        let (reply, receiver) = oneshot::channel();
        if descriptor.byte_len() > MAX_PREPARED_ATTACHMENT_BYTES as u64 {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return BlobReadReceipt {
                receiver: Some(receiver),
                reservation: None,
            };
        }
        let reservation = self.enqueue_blob(BlobJob::Read { descriptor, reply });
        BlobReadReceipt {
            receiver: Some(receiver),
            reservation,
        }
    }

    /// Releases only this explicitly retired blob after committed reference checks.
    pub(crate) fn release_attachment(&self, id: BlobId) -> BlobReleaseReceipt {
        let (reply, receipt) = oneshot::channel();
        drop(self.enqueue_blob(BlobJob::Release { id, reply }));
        receipt
    }

    pub(crate) fn put_queue_chunk(&self, hold: QueueChunkHold, bytes: Vec<u8>) -> BlobReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > QUEUE_CHUNK_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::QueuePut { hold, bytes, reply }));
        receipt
    }

    /// Explicitly resumes only this committed unpublished hold with its original bytes.
    pub(crate) fn resume_queue_chunk(&self, hold: QueueChunkHold, bytes: Vec<u8>) -> BlobReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > QUEUE_CHUNK_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::QueueResume { hold, bytes, reply }));
        receipt
    }

    pub(crate) fn read_queue_chunk(&self, descriptor: BlobDescriptor) -> BlobReadReceipt {
        let (reply, receiver) = oneshot::channel();
        if descriptor.byte_len() > QUEUE_CHUNK_BYTES as u64 {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return BlobReadReceipt {
                receiver: Some(receiver),
                reservation: None,
            };
        }
        let reservation = self.enqueue_blob(BlobJob::QueueRead { descriptor, reply });
        BlobReadReceipt {
            receiver: Some(receiver),
            reservation,
        }
    }

    pub(crate) fn verify_queue_capture(
        &self,
        verification: QueueVerification,
    ) -> QueueVerificationReceipt {
        let (reply, receipt) = oneshot::channel();
        drop(self.enqueue_blob(BlobJob::QueueVerify {
            verification,
            reply,
        }));
        receipt
    }

    /// Captures the exact consumer and edit plan. Foreground acceptance rechecks both.
    pub(crate) fn put_consumer_edit_chunk(
        &self,
        binding: QueueConsumerEditBinding,
        hold: QueueChunkHold,
        bytes: Vec<u8>,
    ) -> QueueConsumerEditChunkReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > QUEUE_CHUNK_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::ConsumerEditPut {
            binding,
            hold,
            bytes,
            reply,
        }));
        receipt
    }

    /// Explicit repair retains the exact consumer and unpublished edit hold.
    pub(crate) fn resume_consumer_edit_chunk(
        &self,
        binding: QueueConsumerEditBinding,
        hold: QueueChunkHold,
        bytes: Vec<u8>,
    ) -> QueueConsumerEditChunkReceipt {
        let (reply, receipt) = oneshot::channel();
        if bytes.len() > QUEUE_CHUNK_BYTES {
            let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::TooLarge,
            ))));
            return receipt;
        }
        drop(self.enqueue_blob(BlobJob::ConsumerEditResume {
            binding,
            hold,
            bytes,
            reply,
        }));
        receipt
    }

    pub(crate) fn verify_consumer_edit_capture(
        &self,
        binding: QueueConsumerEditBinding,
        verification: QueueVerification,
    ) -> QueueConsumerEditVerificationReceipt {
        let (reply, receipt) = oneshot::channel();
        drop(self.enqueue_blob(BlobJob::ConsumerEditVerify {
            binding,
            verification,
            reply,
        }));
        receipt
    }

    /// The worker rechecks the latest committed destination and every retained byte.
    pub(crate) fn authorize_queue_acknowledgement(
        &self,
        operation: QueueOperationId,
        proofs: Vec<QueuePayloadFilesProof>,
    ) -> QueueAcknowledgementReceipt {
        let (reply, receipt) = oneshot::channel();
        drop(self.enqueue_blob(BlobJob::QueueAcknowledge {
            operation,
            proofs,
            reply,
        }));
        receipt
    }

    pub(crate) fn release_queue_chunk(&self, id: BlobId) -> BlobReleaseReceipt {
        let (reply, receipt) = oneshot::channel();
        drop(self.enqueue_blob(BlobJob::QueueRelease { id, reply }));
        receipt
    }

    fn enqueue_blob(&self, job: BlobJob) -> Option<Arc<BlobReservation>> {
        let Ok(mut state) = self.shared.state.lock() else {
            job.fail(BlobFailure::Journal(JournalFailure::WorkerStopped));
            return None;
        };
        let error = phase_error(&state).or_else(|| {
            (state.blob_reservations >= MAX_BLOB_JOBS
                || job.owned_bytes() > MAX_BLOB_BYTES.saturating_sub(state.blob_bytes)
                || state.accepted_blobs == u64::MAX)
                .then_some(JournalFailure::AtCapacity)
        });
        if let Some(error) = error {
            drop(state);
            job.fail(BlobFailure::Journal(error));
            return None;
        }
        if let Err(error) = job.check_authority(state.authority.as_ref()) {
            drop(state);
            job.fail(BlobFailure::Attachment(error));
            return None;
        }
        let reservation = Arc::new(BlobReservation {
            shared: Arc::downgrade(&self.shared),
            bytes: job.owned_bytes(),
        });
        state.blob_bytes += reservation.bytes;
        state.blob_reservations += 1;
        state.accepted_blobs += 1;
        state.blobs.push_back(ReservedBlobJob {
            job,
            reservation: Arc::clone(&reservation),
        });
        drop(state);
        self.shared.wake.notify_one();
        Some(reservation)
    }

    /// Stops accepting work. The receipt resolves after accepted writes finish
    /// and the writer drops, releasing its journal lock. Only the first finish
    /// call is accepted. This dedicated receipt does not consume queue capacity.
    pub(crate) fn finish(&self) -> JournalReceipt {
        let (waiter, receipt) = oneshot::channel();
        let Ok(mut state) = self.shared.state.lock() else {
            let _ = waiter.send(Err(JournalFailure::WorkerStopped));
            return receipt;
        };
        if state.closing {
            let _ = waiter.send(Err(JournalFailure::Closed));
        } else if state.stopped {
            state.closing = true;
            #[cfg(test)]
            if let Some(gate) = state.finish_reply_gate.take() {
                let result = state.last.clone();
                drop(state);
                ReplyGate::hold(&gate, waiter, result);
                return receipt;
            }
            let _ = waiter.send(state.last.clone());
        } else {
            state.closing = true;
            state.finish = Some(waiter);
            self.shared.wake.notify_one();
        }
        receipt
    }
}

impl Drop for JournalWorker {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closing = true;
            self.shared.wake.notify_one();
        }
    }
}

fn admission_error(state: &State) -> Option<JournalFailure> {
    if let Some(error) = phase_error(state) {
        return Some(error);
    }
    let pending = state
        .pending
        .as_ref()
        .map_or(0, |pending| pending.waiters.len());
    if state.active.len() + pending + state.drains.len() >= MAX_WAITERS {
        return Some(JournalFailure::AtCapacity);
    }
    None
}

fn phase_error(state: &State) -> Option<JournalFailure> {
    if state.closing {
        return Some(JournalFailure::Closed);
    }
    match &state.phase {
        Phase::Opening => return Some(JournalFailure::NotHydrated),
        Phase::Failed(error) => return Some(error.clone()),
        Phase::Ready => {}
    }
    if state.stopped {
        return Some(JournalFailure::WorkerStopped);
    }
    None
}

enum Work {
    Snapshot(JournalSnapshot),
    Blob(ReservedBlobJob),
}

#[cfg(test)]
fn execute_blob_with_reply_gate<W>(
    shared: &Shared,
    writer: &mut W,
    job: BlobJob,
    blobs: &mut impl FnMut(&mut W, BlobJob),
) {
    match job {
        BlobJob::QueueVerify {
            verification,
            reply,
        } => {
            let gate = {
                let mut state = shared.state.lock().expect("journal state is not poisoned");
                if state
                    .verification_reply_gate
                    .as_ref()
                    .is_some_and(|(expected, _)| expected == &verification)
                {
                    state.verification_reply_gate.take().map(|(_, gate)| gate)
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                let (computed, mut result) = oneshot::channel();
                blobs(
                    writer,
                    BlobJob::QueueVerify {
                        verification,
                        reply: computed,
                    },
                );
                let result = result
                    .try_recv()
                    .expect("the real synchronous blob callback computed its verification result");
                ReplyGate::hold(&gate, reply, result);
            } else {
                blobs(
                    writer,
                    BlobJob::QueueVerify {
                        verification,
                        reply,
                    },
                );
            }
        }
        BlobJob::ConsumerEditVerify {
            binding,
            verification,
            reply,
        } => {
            let gate = {
                let mut state = shared.state.lock().expect("journal state is not poisoned");
                if state
                    .consumer_edit_verification_reply_gate
                    .as_ref()
                    .is_some_and(|(expected_binding, expected_verification, _)| {
                        expected_binding == &binding && expected_verification == &verification
                    })
                {
                    state
                        .consumer_edit_verification_reply_gate
                        .take()
                        .map(|(_, _, gate)| gate)
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                let (computed, mut result) = oneshot::channel();
                blobs(
                    writer,
                    BlobJob::ConsumerEditVerify {
                        binding,
                        verification,
                        reply: computed,
                    },
                );
                let result = result.try_recv().expect("the real synchronous blob callback computed its consumer edit verification result");
                ReplyGate::hold(&gate, reply, result);
            } else {
                blobs(
                    writer,
                    BlobJob::ConsumerEditVerify {
                        binding,
                        verification,
                        reply,
                    },
                );
            }
        }
        job => blobs(writer, job),
    }
}

fn run<W, O, S, B>(shared: &Shared, open: O, mut save: S, mut blobs: B)
where
    O: FnOnce() -> Result<(W, LoadedJournal), JournalError>,
    S: FnMut(&mut W, &JournalSnapshot) -> Result<JournalCommit, JournalError>,
    B: FnMut(&mut W, BlobJob),
{
    let (mut writer, loaded) = match open() {
        Ok(result) => result,
        Err(error) => {
            stop_failed(shared, error.into());
            return;
        }
    };
    {
        let mut state = shared.state.lock().expect("journal state is not poisoned");
        state.authority = Some(match &loaded {
            LoadedJournal::Missing => AttachmentAuthority::default(),
            LoadedJournal::Present(snapshot) => AttachmentAuthority::capture(snapshot),
        });
        state.phase = Phase::Ready;
        if let Some(hydration) = state.hydration.take() {
            let _ = hydration.send(Ok(loaded));
        }
    }
    loop {
        let work = {
            let mut state = shared.state.lock().expect("journal state is not poisoned");
            while state.pending.is_none() && state.blobs.is_empty() && !state.closing {
                state = shared
                    .wake
                    .wait(state)
                    .expect("journal state is not poisoned");
            }
            if let Some(pending) = state.pending.take() {
                state.active = pending.waiters;
                state.writing = true;
                Work::Snapshot(pending.snapshot)
            } else if let Some(job) = state.blobs.pop_front() {
                Work::Blob(job)
            } else {
                break;
            }
        };
        let snapshot = match work {
            Work::Snapshot(snapshot) => snapshot,
            Work::Blob(ReservedBlobJob { job, reservation }) => {
                // The production callback checks the latest durable authority
                // here, after any prioritized/coalesced journal save completes.
                #[cfg(test)]
                execute_blob_with_reply_gate(shared, &mut writer, job, &mut blobs);
                #[cfg(not(test))]
                blobs(&mut writer, job);
                // The receipt may still own a read buffer. Only the final
                // reservation owner frees its bytes and slot, outside this lock.
                drop(reservation);
                let mut state = shared.state.lock().expect("journal state is not poisoned");
                state.completed_blobs += 1;
                resolve_drains(&mut state);
                continue;
            }
        };
        let outcome = match save(&mut writer, &snapshot) {
            Ok(JournalCommit::Durable) => Ok(CommitOutcome::Durable),
            Ok(JournalCommit::DirectorySyncUncertain(error)) => {
                Ok(CommitOutcome::DirectorySyncUncertain(error.kind()))
            }
            Err(error) => Err(error.into()),
        };
        let authority = (outcome == Ok(CommitOutcome::Durable))
            .then(|| AttachmentAuthority::capture(&snapshot));
        drop(snapshot);
        let mut state = shared.state.lock().expect("journal state is not poisoned");
        state.authority = authority;
        state.last = outcome.clone();
        let (mut failed_pending, canceled_blobs, completed_blobs) =
            if outcome == Ok(CommitOutcome::Durable) {
                (None, VecDeque::new(), state.completed_blobs)
            } else {
                let canceled = std::mem::take(&mut state.blobs);
                (state.pending.take(), canceled, state.accepted_blobs)
            };
        if let Some(pending) = &mut failed_pending {
            state.active.append(&mut pending.waiters);
        }
        // Keep canceled operations charged until their receipts and payloads
        // are drained. Active barriers remain counted throughout cancellation.
        drop(state);
        for ReservedBlobJob { job, reservation } in canceled_blobs {
            job.fail(BlobFailure::Attachment(
                AttachmentFailure::NoDurableAuthority,
            ));
            drop(reservation);
        }
        let mut state = shared.state.lock().expect("journal state is not poisoned");
        state.completed_blobs = completed_blobs;
        state.writing = false;
        // All old pending work was taken before publishing outcomes. A fresh
        // explicit retry accepted during cancellation is never cleared here.
        let waiters = std::mem::take(&mut state.active);
        resolve(&mut state, waiters, &outcome);
        resolve_drains(&mut state);
        drop(state);
        drop(failed_pending);
    }
    drop(writer);
    let mut state = shared.state.lock().expect("journal state is not poisoned");
    state.stopped = true;
    state.authority = None;
    if let Some(waiter) = state.finish.take() {
        #[cfg(test)]
        if let Some(gate) = state.finish_reply_gate.take() {
            let result = state.last.clone();
            drop(state);
            ReplyGate::hold(&gate, waiter, result);
            return;
        }
        let _ = waiter.send(state.last.clone());
    }
}

fn resolve(state: &mut State, waiters: Vec<Barrier>, outcome: &JournalResult) {
    for barrier in waiters {
        if barrier.blobs_through <= state.completed_blobs {
            barrier.complete(outcome.clone());
        } else {
            state.drains.push(ResolvedBarrier {
                barrier,
                outcome: outcome.clone(),
            });
        }
    }
}

fn resolve_drains(state: &mut State) {
    let mut index = 0;
    while index < state.drains.len() {
        if state.drains[index].barrier.blobs_through <= state.completed_blobs {
            let resolved = state.drains.swap_remove(index);
            resolved.barrier.complete(resolved.outcome);
        } else {
            index += 1;
        }
    }
}

fn stop_failed(shared: &Shared, error: JournalFailure) {
    let Ok(mut state) = shared.state.lock() else {
        return;
    };
    state.phase = Phase::Failed(error.clone());
    state.writing = false;
    state.authority = None;
    state.last = Err(error.clone());
    let hydration = state.hydration.take();
    let active = std::mem::take(&mut state.active);
    let drains = std::mem::take(&mut state.drains);
    let pending = state.pending.take();
    let blobs = std::mem::take(&mut state.blobs);
    drop(state);
    if let Some(hydration) = hydration {
        let _ = hydration.send(Err(error.clone()));
    }
    for barrier in active {
        barrier.complete(Err(error.clone()));
    }
    for resolved in drains {
        resolved.barrier.complete(Err(error.clone()));
    }
    if let Some(pending) = pending {
        for barrier in pending.waiters {
            barrier.complete(Err(error.clone()));
        }
    }
    for ReservedBlobJob { job, reservation } in blobs {
        job.fail(BlobFailure::Journal(error.clone()));
        drop(reservation);
    }
    let mut state = shared.state.lock().expect("journal state is not poisoned");
    state.stopped = true;
    if let Some(waiter) = state.finish.take() {
        #[cfg(test)]
        if let Some(gate) = state.finish_reply_gate.take() {
            let result = state.last.clone();
            drop(state);
            ReplyGate::hold(&gate, waiter, result);
            return;
        }
        let _ = waiter.send(state.last.clone());
    }
}

#[cfg(test)]
#[path = "unit_d_journal_gate_tests.rs"]
mod unit_d_gate_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
    use openchamber_native_core::ids::{DraftId, RuntimeId};
    use std::sync::mpsc;

    use crate::mutation_journal::MutationJournal;

    fn snapshot(text: &str) -> JournalSnapshot {
        JournalSnapshot {
            drafts: vec![(
                DraftKey::new(
                    RuntimeId::from("runtime"),
                    "/project",
                    DraftTarget::NewSession(DraftId::from("draft")),
                )
                .expect("valid owner"),
                Draft::new(text),
            )],
            attempts: Vec::new(),
            creations: Vec::new(),
            selections: Vec::new(),
            ..JournalSnapshot::default()
        }
    }

    fn receive(receipt: JournalReceipt) -> JournalResult {
        receipt.blocking_recv().expect("worker answers receipt")
    }

    fn blob_id(value: u128) -> BlobId {
        BlobId::parse(&format!("{value:032x}")).expect("fixed test blob label")
    }

    fn preparation(value: u128) -> PreparationHold {
        PreparationHold {
            owner: snapshot("").drafts.remove(0).0,
            id: AttachmentId::from(blob_id(value).as_str()),
            generation: 1,
            blob: blob_id(value),
        }
    }

    fn put(worker: &JournalWorker, hold: &PreparationHold, bytes: Vec<u8>) -> BlobReceipt {
        worker.put_attachment(
            hold.owner.clone(),
            hold.id.clone(),
            hold.generation,
            hold.blob,
            bytes,
        )
    }

    fn receive_blob<T>(
        receipt: oneshot::Receiver<Result<T, BlobFailure>>,
    ) -> Result<T, BlobFailure> {
        receipt
            .blocking_recv()
            .expect("worker answers blob receipt")
    }

    fn receive_read(receipt: BlobReadReceipt) -> Result<BlobBytes, BlobFailure> {
        receipt
            .blocking_recv()
            .expect("worker answers read receipt")
    }

    fn read_snapshot(descriptor: BlobDescriptor) -> JournalSnapshot {
        let mut snapshot = JournalSnapshot::default();
        snapshot.attachments.insert(
            AttachmentId::from(descriptor.id().as_str()),
            crate::attachment_catalog::PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        snapshot
    }

    fn answer_fake(job: BlobJob, commit: SpoolCommit) {
        match job {
            BlobJob::Put { hold, bytes, reply } => {
                let _ = reply.send(Ok(SpoolBlobReceipt {
                    descriptor: BlobDescriptor::new(hold.blob, bytes.len() as u64, [0; 32]),
                    commit,
                }));
            }
            BlobJob::Read { reply, .. } => {
                let _ = reply.send(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                    SpoolError::Io(io::ErrorKind::NotFound),
                ))));
            }
            BlobJob::Release { reply, .. } => {
                let _ = reply.send(Ok(commit));
            }
            queue_job => queue_job.fail(BlobFailure::Attachment(AttachmentFailure::Unauthorized)),
        }
    }

    #[gpui_kit::test]
    async fn cancelled_finish_reply_gates_drop_replies_before_or_after_reaching_without_blocking_worker(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.executor().allow_parking();
        for cancel_before_completion in [true, false] {
            struct Writer(mpsc::Sender<()>);
            impl Drop for Writer {
                fn drop(&mut self) {
                    let _ = self.0.send(());
                }
            }
            let (dropped, drop_notice) = mpsc::channel();
            let (worker, hydration) = JournalWorker::start_with(
                move || Ok((Writer(dropped), LoadedJournal::Missing)),
                |_, _| Ok(JournalCommit::Durable),
            );
            hydration
                .await
                .expect("worker hydrates")
                .expect("worker opens");
            let mut gate = worker.hold_finish_reply_for_test();
            if cancel_before_completion {
                gate.cancel();
            }
            let receipt = worker.finish();
            gate.reached().await;
            drop_notice
                .try_recv()
                .expect("a held or cancelled reply never delays dropping the worker's writer");
            assert!(gate.has_reached());
            if !cancel_before_completion {
                gate.cancel();
            }
            gate.release();
            assert!(
                receipt.await.is_err(),
                "cancellation drops the computed result and cannot be undone by release"
            );
            assert_eq!(
                worker.finish().await.expect("stopped worker still answers"),
                Err(JournalFailure::Closed)
            );
        }
    }

    #[gpui_kit::test]
    async fn held_finish_failure_reply_keeps_its_actual_result_and_drop_cancels_other_held_replies(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            |_, _| Err(JournalError::Invalid("test write rejected")),
        );
        hydration
            .await
            .expect("worker hydrates")
            .expect("worker opens");
        let failure = Err(JournalFailure::Invalid("test write rejected"));
        assert_eq!(
            worker
                .submit(snapshot("rejected"))
                .await
                .expect("real save returns"),
            failure
        );
        let mut gate = worker.hold_finish_reply_for_test();
        let mut receipt = worker.finish();
        gate.reached().await;
        assert!(
            matches!(receipt.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "the actual failure reply is held after the writer drops"
        );
        gate.release();
        assert_eq!(receipt.await.expect("released reply returns"), failure);

        let gate = ReplyGate::new();
        let (reply, receipt) = oneshot::channel();
        ReplyGate::hold(&gate.state, reply, failure);
        assert!(gate.has_reached());
        drop(gate);
        assert!(
            receipt.await.is_err(),
            "dropping an unreleased gate cancels its held sender and result"
        );
    }

    #[test]
    fn blob_work_and_coalesced_snapshots_share_one_thread_and_flush_and_finish_drain() {
        struct Writer {
            owner: thread::ThreadId,
            dropped: mpsc::Sender<()>,
        }
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.dropped.send(());
            }
        }
        let hold = preparation(1);
        let descriptor = BlobDescriptor::new(blob_id(2), 4, [0; 32]);
        let mut initial = snapshot("initial");
        initial.preparations.push(hold.clone());
        initial.attachments.insert(
            AttachmentId::from(blob_id(2).as_str()),
            crate::attachment_catalog::PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        initial.retired_blobs.push(blob_id(3));
        let mut latest = snapshot("latest");
        latest.preparations = initial.preparations.clone();
        latest.attachments = initial.attachments.clone();
        latest.retired_blobs = initial.retired_blobs.clone();
        let (dropped, drop_notice) = mpsc::channel();
        let (entered, entries) = mpsc::channel();
        let save_entered = entered.clone();
        let (release, permits) = mpsc::channel();
        let caller = thread::current().id();
        let (worker, hydration) = JournalWorker::start_with_io(
            move || {
                Ok((
                    Writer {
                        owner: thread::current().id(),
                        dropped,
                    },
                    LoadedJournal::Present(Box::new(initial)),
                ))
            },
            move |writer, snapshot| {
                assert_eq!(writer.owner, thread::current().id());
                save_entered
                    .send(snapshot.drafts[0].1.text.clone())
                    .expect("save entry");
                Ok(JournalCommit::Durable)
            },
            move |writer, job| {
                assert_eq!(writer.owner, thread::current().id());
                assert_ne!(caller, thread::current().id());
                let name = match &job {
                    BlobJob::Put { .. } => "put",
                    BlobJob::AttachmentResume { .. } => "attachment-resume",
                    BlobJob::DownloadPut { .. } => "download-put",
                    BlobJob::Read { .. } => "read",
                    BlobJob::Release { .. } => "release",
                    BlobJob::QueuePut { .. } => "queue-put",
                    BlobJob::QueueResume { .. } => "queue-resume",
                    BlobJob::QueueRead { .. } => "queue-read",
                    BlobJob::QueueVerify { .. } => "queue-verify",
                    BlobJob::ConsumerEditPut { .. } => "consumer-edit-put",
                    BlobJob::ConsumerEditResume { .. } => "consumer-edit-resume",
                    BlobJob::ConsumerEditVerify { .. } => "consumer-edit-verify",
                    BlobJob::QueueAcknowledge { .. } => "queue-acknowledge",
                    BlobJob::QueueRelease { .. } => "queue-release",
                };
                entered.send(name.to_owned()).expect("blob entry");
                permits.recv().expect("release blob");
                answer_fake(
                    job,
                    SpoolCommit::DirectorySyncUncertain(io::ErrorKind::Other),
                );
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = put(&worker, &hold, b"saved".to_vec());
        assert_eq!(entries.recv().expect("active put"), "put");
        let read = worker.read_attachment(descriptor);
        let released = worker.release_attachment(blob_id(3));
        let superseded = worker.submit(snapshot("superseded"));
        let saved = worker.submit(latest);
        let mut flush = worker.flush();
        let mut finish = worker.finish();
        assert_eq!(
            receive_blob(worker.release_attachment(blob_id(3))),
            Err(BlobFailure::Journal(JournalFailure::Closed))
        );
        assert!(matches!(
            flush.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release put");
        assert_eq!(
            receive_blob(first).expect("put receipt").commit,
            SpoolCommit::DirectorySyncUncertain(io::ErrorKind::Other)
        );
        assert_eq!(entries.recv().expect("latest save"), "latest");
        assert_eq!(receive(superseded), Ok(CommitOutcome::Durable));
        assert_eq!(receive(saved), Ok(CommitOutcome::Durable));
        assert_eq!(entries.recv().expect("read after save"), "read");
        assert!(matches!(
            flush.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release missing read");
        assert!(matches!(
            receive_read(read),
            Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Io(io::ErrorKind::NotFound)
            )))
        ));
        assert_eq!(entries.recv().expect("release after read"), "release");
        assert!(matches!(
            finish.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release final job");
        assert_eq!(
            receive_blob(released),
            Ok(SpoolCommit::DirectorySyncUncertain(io::ErrorKind::Other))
        );
        assert_eq!(
            receive(flush),
            Ok(CommitOutcome::Durable),
            "blob errors preserve journal outcome"
        );
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
        drop_notice
            .try_recv()
            .expect("writer dropped before finish");
    }

    #[test]
    fn dropping_handle_drains_attachment_jobs_before_releasing_writer() {
        struct Writer(mpsc::Sender<()>);
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let initial = JournalSnapshot {
            retired_blobs: vec![blob_id(1), blob_id(2)],
            ..JournalSnapshot::default()
        };
        let (dropped, drop_notice) = mpsc::channel();
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with_io(
            move || Ok((Writer(dropped), LoadedJournal::Present(Box::new(initial)))),
            |_, _| panic!("no snapshot requested"),
            move |_, job| {
                entered.send(()).expect("blob entry");
                permits.recv().expect("release blob");
                answer_fake(job, SpoolCommit::Durable);
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = worker.release_attachment(blob_id(1));
        entries.recv().expect("first job entered");
        let second = worker.release_attachment(blob_id(2));
        drop(worker);
        release.send(()).expect("release first job");
        entries.recv().expect("second job entered");
        assert!(matches!(
            drop_notice.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).expect("release second job");
        assert_eq!(receive_blob(first), Ok(SpoolCommit::Durable));
        assert_eq!(receive_blob(second), Ok(SpoolCommit::Durable));
        drop_notice.recv().expect("writer released after drain");
    }

    #[test]
    fn blob_count_capacity_does_not_consume_snapshot_capacity() {
        let initial = JournalSnapshot {
            retired_blobs: vec![blob_id(1)],
            ..JournalSnapshot::default()
        };
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let mut first = true;
        let (worker, hydration) = JournalWorker::start_with_io(
            move || Ok(((), LoadedJournal::Present(Box::new(initial)))),
            |_, _| Ok(JournalCommit::Durable),
            move |_, job| {
                if first {
                    first = false;
                    entered.send(()).expect("first entered");
                    permit.recv().expect("release first");
                }
                answer_fake(job, SpoolCommit::Durable);
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let mut receipts = vec![worker.release_attachment(blob_id(1))];
        entry.recv().expect("active blob counts toward capacity");
        for _ in 1..MAX_BLOB_JOBS {
            receipts.push(worker.release_attachment(blob_id(1)));
        }
        assert_eq!(
            receive_blob(worker.release_attachment(blob_id(1))),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity))
        );
        let mut next = snapshot("unrelated full snapshot");
        next.retired_blobs.push(blob_id(1));
        let saved = worker.submit(next);
        let finish = worker.finish();
        release.send(()).expect("release active blob");
        assert_eq!(receive(saved), Ok(CommitOutcome::Durable));
        for receipt in receipts {
            assert_eq!(receive_blob(receipt), Ok(SpoolCommit::Durable));
        }
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn blob_byte_capacity_includes_active_buffers_and_rejects_oversized_operations() {
        let hold = preparation(1);
        let other = preparation(3);
        let descriptor = BlobDescriptor::new(blob_id(2), 1, [0; 32]);
        let mut initial = JournalSnapshot {
            preparations: vec![hold.clone(), other.clone()],
            ..JournalSnapshot::default()
        };
        initial.attachments.insert(
            AttachmentId::from(blob_id(2).as_str()),
            crate::attachment_catalog::PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let mut first = true;
        let (worker, hydration) = JournalWorker::start_with_io(
            move || Ok(((), LoadedJournal::Present(Box::new(initial)))),
            |_, _| Ok(JournalCommit::Durable),
            move |_, job| {
                if first {
                    first = false;
                    entered.send(()).expect("active put");
                    permit.recv().expect("release put");
                }
                answer_fake(job, SpoolCommit::Durable);
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = put(&worker, &hold, vec![0; MAX_PREPARED_ATTACHMENT_BYTES]);
        entry.recv().expect("first allocation active");
        assert_eq!(
            receive_blob(put(&worker, &other, vec![1; MAX_PREPARED_ATTACHMENT_BYTES])),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity)),
            "two full payloads must also leave room for comparison workspaces"
        );
        let second = put(
            &worker,
            &other,
            vec![1; MAX_PREPARED_ATTACHMENT_BYTES - 2 * PUT_COMPARE_BYTES],
        );
        assert_eq!(
            worker.shared.state.lock().expect("test state").blob_bytes,
            MAX_BLOB_BYTES,
            "both comparison workspaces count toward the exact buffer ceiling"
        );
        assert_eq!(
            receive_blob(put(&worker, &hold, vec![2])),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity))
        );
        assert!(matches!(
            receive_read(worker.read_attachment(descriptor)),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity))
        ));
        let too_large = BlobFailure::Attachment(AttachmentFailure::Spool(SpoolError::TooLarge));
        assert_eq!(
            receive_blob(put(
                &worker,
                &hold,
                vec![0; MAX_PREPARED_ATTACHMENT_BYTES + 1]
            )),
            Err(too_large.clone())
        );
        assert!(
            matches!(receive_read(worker.read_attachment(BlobDescriptor::new(blob_id(2), MAX_PREPARED_ATTACHMENT_BYTES as u64 + 1, [0; 32]))), Err(error) if error == too_large)
        );
        let mut next = snapshot("full snapshot still admitted");
        next.preparations = vec![hold.clone(), other];
        next.attachments.insert(
            AttachmentId::from(blob_id(2).as_str()),
            crate::attachment_catalog::PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        let saved = worker.submit(next);
        let finish = worker.finish();
        release.send(()).expect("release active put");
        assert!(receive_blob(first).is_ok());
        assert!(receive_blob(second).is_ok());
        assert_eq!(receive(saved), Ok(CommitOutcome::Durable));
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn read_admission_charges_oversize_sentinels_at_the_exact_buffer_boundary() {
        let maximum =
            BlobDescriptor::new(blob_id(1), MAX_PREPARED_ATTACHMENT_BYTES as u64, [0; 32]);
        let remaining = BlobDescriptor::new(
            blob_id(2),
            (MAX_PREPARED_ATTACHMENT_BYTES - 2) as u64,
            [0; 32],
        );
        let empty = BlobDescriptor::new(blob_id(3), 0, [0; 32]);
        let mut initial = JournalSnapshot::default();
        for descriptor in [maximum, remaining, empty] {
            initial.attachments.insert(
                AttachmentId::from(descriptor.id().as_str()),
                crate::attachment_catalog::PreparedAttachment {
                    blob: descriptor,
                    staging: None,
                },
            );
        }
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let mut first = true;
        let (worker, hydration) = JournalWorker::start_with_io(
            move || Ok(((), LoadedJournal::Present(Box::new(initial)))),
            |_, _| Ok(JournalCommit::Durable),
            move |_, job| {
                if first {
                    first = false;
                    entered.send(()).expect("active read");
                    permit.recv().expect("release read");
                }
                answer_fake(job, SpoolCommit::Durable);
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = worker.read_attachment(maximum);
        entry.recv().expect("maximum read active");
        assert!(
            matches!(
                receive_read(worker.read_attachment(maximum)),
                Err(BlobFailure::Journal(JournalFailure::AtCapacity))
            ),
            "two maximum reads exceed the ceiling by their sentinel bytes"
        );
        let second = worker.read_attachment(remaining);
        assert_eq!(
            worker.shared.state.lock().expect("test state").blob_bytes,
            MAX_BLOB_BYTES
        );
        assert!(
            matches!(
                receive_read(worker.read_attachment(empty)),
                Err(BlobFailure::Journal(JournalFailure::AtCapacity))
            ),
            "even an empty read reserves its oversize sentinel"
        );
        let flush = worker.flush();
        let finish = worker.finish();
        release.send(()).expect("release active read");
        for receipt in [first, second] {
            assert!(matches!(
                receive_read(receipt),
                Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                    SpoolError::Io(io::ErrorKind::NotFound)
                )))
            ));
        }
        assert_eq!(receive(flush), Ok(CommitOutcome::Durable));
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn dropping_read_receipt_early_keeps_active_bytes_charged_until_io_finishes() {
        let descriptor =
            BlobDescriptor::new(blob_id(1), MAX_PREPARED_ATTACHMENT_BYTES as u64, [0; 32]);
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let mut first = true;
        let (worker, hydration) = JournalWorker::start_with_io(
            move || {
                Ok((
                    (),
                    LoadedJournal::Present(Box::new(read_snapshot(descriptor))),
                ))
            },
            |_, _| Ok(JournalCommit::Durable),
            move |_, job| {
                if first {
                    first = false;
                    entered.send(()).expect("active read");
                    permit.recv().expect("release active read");
                }
                answer_fake(job, SpoolCommit::Durable);
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let receipt = worker.read_attachment(descriptor);
        entry.recv().expect("read started");
        drop(receipt);
        assert!(matches!(
            receive_read(worker.read_attachment(descriptor)),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity))
        ));
        assert_eq!(
            worker.shared.state.lock().expect("test state").blob_bytes,
            MAX_PREPARED_ATTACHMENT_BYTES + 1
        );
        let flush = worker.flush();
        release.send(()).expect("complete abandoned read");
        assert_eq!(receive(flush), Ok(CommitOutcome::Durable));
        assert_eq!(
            worker.shared.state.lock().expect("test state").blob_bytes,
            0
        );
        let mut next = worker.read_attachment(descriptor);
        assert_eq!(receive(worker.flush()), Ok(CommitOutcome::Durable));
        assert!(matches!(
            next.try_recv(),
            Ok(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Io(io::ErrorKind::NotFound)
            ))))
        ));
        assert_eq!(
            worker
                .shared
                .state
                .lock()
                .expect("test state")
                .blob_reservations,
            0
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn completed_unread_reads_keep_count_slots_but_never_block_finish() {
        let descriptor = BlobDescriptor::new(blob_id(1), 0, [0; 32]);
        let (worker, hydration) = JournalWorker::start_with_io(
            move || {
                Ok((
                    (),
                    LoadedJournal::Present(Box::new(read_snapshot(descriptor))),
                ))
            },
            |_, _| Ok(JournalCommit::Durable),
            |_, job| answer_fake(job, SpoolCommit::Durable),
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let mut receipts: Vec<_> = (0..MAX_BLOB_JOBS)
            .map(|_| worker.read_attachment(descriptor))
            .collect();
        assert_eq!(receive(worker.flush()), Ok(CommitOutcome::Durable));
        assert!(matches!(
            receive_read(worker.read_attachment(descriptor)),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity))
        ));
        drop(receipts.pop().expect("one unread receipt"));
        receipts.push(worker.read_attachment(descriptor));
        assert_eq!(
            receive(worker.submit(read_snapshot(descriptor))),
            Ok(CommitOutcome::Durable),
            "unread file-error replies never consume journal admission capacity"
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        assert_eq!(
            worker
                .shared
                .state
                .lock()
                .expect("test state")
                .blob_reservations,
            MAX_BLOB_JOBS
        );
        drop(receipts);
        let state = worker.shared.state.lock().expect("test state");
        assert_eq!(state.blob_reservations, 0);
        assert_eq!(state.blob_bytes, 0);
    }

    #[test]
    fn consuming_a_ready_reply_before_job_return_keeps_its_active_reservation() {
        let descriptor =
            BlobDescriptor::new(blob_id(1), MAX_PREPARED_ATTACHMENT_BYTES as u64, [0; 32]);
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let mut first = true;
        let (worker, hydration) = JournalWorker::start_with_io(
            move || {
                Ok((
                    (),
                    LoadedJournal::Present(Box::new(read_snapshot(descriptor))),
                ))
            },
            |_, _| Ok(JournalCommit::Durable),
            move |_, job| {
                answer_fake(job, SpoolCommit::Durable);
                if first {
                    first = false;
                    entered.send(()).expect("reply delivered before job return");
                    permit.recv().expect("release active job");
                }
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let mut receipt = worker.read_attachment(descriptor);
        entry
            .recv()
            .expect("reply is ready while the job still runs");
        assert!(matches!(
            receipt.try_recv(),
            Ok(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::Io(io::ErrorKind::NotFound)
            ))))
        ));
        assert!(matches!(
            receive_read(worker.read_attachment(descriptor)),
            Err(BlobFailure::Journal(JournalFailure::AtCapacity))
        ));
        let flush = worker.flush();
        release.send(()).expect("complete the active job");
        assert_eq!(receive(flush), Ok(CommitOutcome::Durable));
        assert_eq!(
            worker.shared.state.lock().expect("test state").blob_bytes,
            0
        );
        drop(worker.read_attachment(descriptor));
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        assert_eq!(
            worker
                .shared
                .state
                .lock()
                .expect("test state")
                .blob_reservations,
            0
        );
    }

    #[test]
    fn journal_failure_releases_canceled_read_lease_after_early_receipt_drop() {
        let descriptor =
            BlobDescriptor::new(blob_id(1), MAX_PREPARED_ATTACHMENT_BYTES as u64, [0; 32]);
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with_io(
            move || {
                Ok((
                    (),
                    LoadedJournal::Present(Box::new(read_snapshot(descriptor))),
                ))
            },
            move |_, _| {
                entered.send(()).expect("save entered");
                permit.recv().expect("release failing save");
                Err(JournalError::Io(io::Error::other("private diagnostic")))
            },
            |_, _| panic!("canceled read cannot execute"),
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let saved = worker.submit(snapshot("failure"));
        entry.recv().expect("save active");
        drop(worker.read_attachment(descriptor));
        assert_eq!(
            worker
                .shared
                .state
                .lock()
                .expect("test state")
                .blob_reservations,
            1
        );
        let flush = worker.flush();
        release.send(()).expect("fail journal");
        assert_eq!(
            receive(flush),
            Err(JournalFailure::Io(io::ErrorKind::Other))
        );
        assert_eq!(
            receive(saved),
            Err(JournalFailure::Io(io::ErrorKind::Other))
        );
        assert_eq!(
            receive(worker.finish()),
            Err(JournalFailure::Io(io::ErrorKind::Other))
        );
        let state = worker.shared.state.lock().expect("test state");
        assert_eq!(state.blob_reservations, 0);
        assert_eq!(state.blob_bytes, 0);
    }

    #[test]
    fn panicked_read_retains_only_its_unconsumed_receipt_lease() {
        let descriptor = BlobDescriptor::new(blob_id(1), 0, [0; 32]);
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with_io(
            move || {
                Ok((
                    (),
                    LoadedJournal::Present(Box::new(read_snapshot(descriptor))),
                ))
            },
            |_, _| panic!("no save requested"),
            move |_, _job| {
                entered.send(()).expect("read entered");
                permit.recv().expect("release panic");
                panic!("injected read panic");
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let mut active = worker.read_attachment(descriptor);
        entry.recv().expect("read active");
        drop(worker.read_attachment(descriptor));
        let finish = worker.finish();
        release.send(()).expect("panic worker");
        assert_eq!(receive(finish), Err(JournalFailure::WorkerStopped));
        assert_eq!(
            worker
                .shared
                .state
                .lock()
                .expect("test state")
                .blob_reservations,
            1
        );
        assert!(matches!(
            active.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let state = worker.shared.state.lock().expect("test state");
        assert_eq!(state.blob_reservations, 0);
        assert_eq!(state.blob_bytes, 0);
    }

    #[test]
    fn deferred_flush_barriers_keep_the_existing_waiter_bound() {
        let initial = JournalSnapshot {
            retired_blobs: vec![blob_id(1)],
            ..JournalSnapshot::default()
        };
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with_io(
            move || Ok(((), LoadedJournal::Present(Box::new(initial)))),
            |_, _| panic!("snapshot was rejected"),
            move |_, job| {
                entered.send(()).expect("blob entered");
                permit.recv().expect("release blob");
                answer_fake(job, SpoolCommit::Durable);
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let blob = worker.release_attachment(blob_id(1));
        entry.recv().expect("blob is active");
        let receipts: Vec<_> = (0..MAX_WAITERS).map(|_| worker.flush()).collect();
        assert_eq!(receive(worker.flush()), Err(JournalFailure::AtCapacity));
        assert_eq!(
            receive(worker.submit(snapshot("rejected"))),
            Err(JournalFailure::AtCapacity)
        );
        let finish = worker.finish();
        release.send(()).expect("release blob");
        assert_eq!(receive_blob(blob), Ok(SpoolCommit::Durable));
        for receipt in receipts {
            assert_eq!(receive(receipt), Ok(CommitOutcome::Durable));
        }
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn failed_or_uncertain_journal_save_cancels_queued_blob_work_before_flush() {
        for uncertain in [false, true] {
            let hold = preparation(1);
            let initial = JournalSnapshot {
                preparations: vec![hold.clone()],
                ..JournalSnapshot::default()
            };
            let (entered, entry) = mpsc::channel();
            let (release, permit) = mpsc::channel();
            let mut first = true;
            let (worker, hydration) = JournalWorker::start_with_io(
                move || Ok(((), LoadedJournal::Present(Box::new(initial)))),
                move |_, _| {
                    if !first {
                        return Ok(JournalCommit::Durable);
                    }
                    first = false;
                    entered.send(()).expect("save entered");
                    permit.recv().expect("release save failure");
                    if uncertain {
                        Ok(JournalCommit::DirectorySyncUncertain(io::Error::other(
                            "private",
                        )))
                    } else {
                        Err(JournalError::Io(io::Error::other("private")))
                    }
                },
                |_, _| panic!("canceled blob must never execute"),
            );
            assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
            let saved = worker.submit(snapshot("failure"));
            entry.recv().expect("save active");
            let mut blob = put(&worker, &hold, b"captured".to_vec());
            let pending = worker.submit(snapshot("canceled pending"));
            let flush = worker.flush();
            release.send(()).expect("fail journal");
            let expected = if uncertain {
                Ok(CommitOutcome::DirectorySyncUncertain(io::ErrorKind::Other))
            } else {
                Err(JournalFailure::Io(io::ErrorKind::Other))
            };
            assert_eq!(receive(flush), expected);
            assert_eq!(
                blob.try_recv(),
                Ok(Err(BlobFailure::Attachment(
                    AttachmentFailure::NoDurableAuthority
                ))),
                "flush includes canceled blob receipt"
            );
            assert_eq!(receive(saved), expected);
            assert_eq!(receive(pending), expected);
            assert_eq!(
                receive_blob(put(&worker, &hold, Vec::new())),
                Err(BlobFailure::Attachment(
                    AttachmentFailure::NoDurableAuthority
                ))
            );
            assert_eq!(
                receive(worker.submit(snapshot("explicit retry"))),
                Ok(CommitOutcome::Durable)
            );
            assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        }
    }

    #[test]
    fn future_pending_intent_cannot_admit_put_and_later_removal_cancels_stale_job() {
        let hold = preparation(1);
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with_io(
            || Ok((false, LoadedJournal::Missing)),
            move |authority, snapshot| {
                entered.send(()).expect("save entered");
                permits.recv().expect("release save");
                *authority = !snapshot.preparations.is_empty();
                Ok(JournalCommit::Durable)
            },
            move |authority, job| {
                assert!(
                    !*authority,
                    "latest committed removal prevents file mutation"
                );
                job.fail(BlobFailure::Attachment(AttachmentFailure::Unauthorized));
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let mut intent = snapshot("hold");
        intent.preparations.push(hold.clone());
        let saved = worker.submit(intent);
        entries.recv().expect("intent pending durability");
        assert_eq!(
            receive_blob(put(&worker, &hold, b"too early".to_vec())),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        release.send(()).expect("commit intent");
        assert_eq!(receive(saved), Ok(CommitOutcome::Durable));
        let removed = worker.submit(snapshot("remove hold"));
        entries.recv().expect("removal saving");
        let stale = put(&worker, &hold, b"stale".to_vec());
        release.send(()).expect("commit removal before blob");
        assert_eq!(receive(removed), Ok(CommitOutcome::Durable));
        assert_eq!(
            receive_blob(stale),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn panicked_blob_job_cancels_queued_receipts_before_finish_answers() {
        let initial = JournalSnapshot {
            retired_blobs: vec![blob_id(1)],
            ..JournalSnapshot::default()
        };
        let (entered, entry) = mpsc::channel();
        let (release, permit) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with_io(
            move || Ok(((), LoadedJournal::Present(Box::new(initial)))),
            |_, _| panic!("no journal save requested"),
            move |_, _job| {
                entered.send(()).expect("blob entered");
                permit.recv().expect("release panic");
                panic!("injected blob worker failure");
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let active = worker.release_attachment(blob_id(1));
        entry.recv().expect("first job active");
        let mut queued = worker.release_attachment(blob_id(1));
        let flush = worker.flush();
        let finish = worker.finish();
        release.send(()).expect("panic active job");
        assert_eq!(receive(finish), Err(JournalFailure::WorkerStopped));
        assert_eq!(
            queued.try_recv(),
            Ok(Err(BlobFailure::Journal(JournalFailure::WorkerStopped)))
        );
        assert!(
            active.blocking_recv().is_err(),
            "panic cannot confirm active operation"
        );
        assert_eq!(receive(flush), Err(JournalFailure::WorkerStopped));
    }

    #[test]
    fn coalescing_preserves_order_and_all_barriers_and_finish_drains() {
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            move |_, snapshot| {
                entered
                    .send(snapshot.drafts[0].1.text.clone())
                    .expect("test receiver");
                permits.recv().expect("test release");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Ok(LoadedJournal::Missing))
        ));
        let first = worker.submit(snapshot("first"));
        assert_eq!(entries.recv().expect("first started"), "first");
        let mut second = worker.submit(snapshot("second"));
        let mut third = worker.submit(snapshot("third"));
        let flush = worker.flush();
        let finish = worker.finish();
        assert_eq!(
            receive(worker.submit(snapshot("rejected"))),
            Err(JournalFailure::Closed)
        );
        release.send(()).expect("release first");
        assert_eq!(receive(first), Ok(CommitOutcome::Durable));
        assert_eq!(entries.recv().expect("latest pending started"), "third");
        assert!(matches!(
            second.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            third.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release third");
        for receipt in [second, third, flush, finish] {
            assert_eq!(receive(receipt), Ok(CommitOutcome::Durable));
        }
        assert!(
            entries.recv().is_err(),
            "writer stopped after exactly two saves"
        );
    }

    #[test]
    fn prehydration_and_open_failure_fail_closed() {
        let (release, permit) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            move || {
                permit.recv().expect("release open");
                Err::<((), LoadedJournal), _>(JournalError::Locked)
            },
            |_, _| panic!("failed open cannot save"),
        );
        assert_eq!(
            receive(worker.submit(snapshot("early"))),
            Err(JournalFailure::NotHydrated)
        );
        release.send(()).expect("release open failure");
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Err(JournalFailure::Locked))
        ));
        assert_eq!(
            receive(worker.submit(snapshot("late"))),
            Err(JournalFailure::Locked)
        );
        assert_eq!(receive(worker.finish()), Err(JournalFailure::Locked));
    }

    #[test]
    fn failures_cancel_pending_barriers_and_allow_explicit_retry() {
        for uncertain in [false, true] {
            let (entered, entries) = mpsc::channel();
            let (release, permits) = mpsc::channel();
            let mut writes = 0;
            let (worker, hydration) = JournalWorker::start_with(
                || Ok(((), LoadedJournal::Missing)),
                move |_, _| {
                    writes += 1;
                    if writes == 1 {
                        entered.send(()).expect("entered");
                        permits.recv().expect("release failure");
                        if uncertain {
                            return Ok(JournalCommit::DirectorySyncUncertain(io::Error::other(
                                "private context",
                            )));
                        }
                        return Err(JournalError::Io(io::Error::other("private context")));
                    }
                    Ok(JournalCommit::Durable)
                },
            );
            assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
            let first = worker.submit(snapshot("first"));
            entries.recv().expect("first writing");
            let pending = worker.submit(snapshot("pending"));
            let flush = worker.flush();
            release.send(()).expect("fail write");
            let expected = if uncertain {
                Ok(CommitOutcome::DirectorySyncUncertain(io::ErrorKind::Other))
            } else {
                Err(JournalFailure::Io(io::ErrorKind::Other))
            };
            for receipt in [first, pending, flush] {
                assert_eq!(receive(receipt), expected);
            }
            assert_eq!(receive(worker.flush()), expected);
            assert_eq!(
                receive(worker.submit(snapshot("explicit retry"))),
                Ok(CommitOutcome::Durable)
            );
            assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        }
    }

    #[test]
    fn waiter_capacity_rejects_without_replacing_pending_snapshot() {
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            move |_, snapshot| {
                entered
                    .send(snapshot.drafts[0].1.text.clone())
                    .expect("entry");
                permits.recv().expect("release");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = worker.submit(snapshot("first"));
        entries.recv().expect("first entered");
        let mut receipts = vec![first, worker.submit(snapshot("retained"))];
        for _ in 2..MAX_WAITERS {
            receipts.push(worker.flush());
        }
        assert_eq!(
            receive(worker.submit(snapshot("rejected"))),
            Err(JournalFailure::AtCapacity)
        );
        assert_eq!(receive(worker.flush()), Err(JournalFailure::AtCapacity));
        let finish = worker.finish();
        release.send(()).expect("release first");
        assert_eq!(entries.recv().expect("retained pending"), "retained");
        release.send(()).expect("release pending");
        for receipt in receipts {
            assert_eq!(receive(receipt), Ok(CommitOutcome::Durable));
        }
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn dropping_handle_drains_pending_work_and_releases_writer() {
        struct Writer(mpsc::Sender<()>);
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (dropped, drop_notice) = mpsc::channel();
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            move || Ok((Writer(dropped), LoadedJournal::Missing)),
            move |_, _| {
                entered.send(()).expect("entered");
                permits.recv().expect("release");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = worker.submit(snapshot("first"));
        entries.recv().expect("first entered");
        let second = worker.submit(snapshot("second"));
        drop(worker);
        release.send(()).expect("release first");
        entries.recv().expect("second entered");
        release.send(()).expect("release second");
        assert_eq!(receive(first), Ok(CommitOutcome::Durable));
        assert_eq!(receive(second), Ok(CommitOutcome::Durable));
        drop_notice
            .recv()
            .expect("writer dropped after pending commit");
    }

    #[test]
    fn finish_receipt_releases_real_journal_lock() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = PathBuf::from(format!(
            "/tmp/opencode/journal-worker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("journal.json");
        let (worker, hydration) = JournalWorker::start(path.clone());
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Ok(LoadedJournal::Missing))
        ));
        assert!(matches!(
            MutationJournal::open(&path),
            Err(JournalError::Locked)
        ));
        assert_eq!(
            receive(worker.submit(snapshot("persisted"))),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        let (journal, loaded) = MutationJournal::open(&path).expect("finish released lock");
        let LoadedJournal::Present(snapshot) = loaded else {
            panic!("saved journal exists");
        };
        assert_eq!(snapshot.drafts[0].1.text, "persisted");
        drop(journal);
        std::fs::remove_dir_all(directory).expect("remove test journal");
    }

    #[test]
    fn flush_waits_for_inflight_work_and_finish_preserves_its_failure() {
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            move |_, _| {
                entered.send(()).expect("write entered");
                permits.recv().expect("release write");
                Err(JournalError::Invalid("invalid snapshot"))
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let write = worker.submit(snapshot("first"));
        entries.recv().expect("write started");
        let mut flush = worker.flush();
        assert!(matches!(
            flush.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let finish = worker.finish();
        release.send(()).expect("release failing write");
        for receipt in [write, flush, finish] {
            assert_eq!(
                receive(receipt),
                Err(JournalFailure::Invalid("invalid snapshot"))
            );
        }
    }

    #[test]
    fn finish_during_open_waits_until_writer_is_released() {
        struct Writer(mpsc::Sender<()>);
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (release, permit) = mpsc::channel();
        let (dropped, drop_notice) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            move || {
                permit.recv().expect("release open");
                Ok((Writer(dropped), LoadedJournal::Missing))
            },
            |_, _| panic!("no snapshots were accepted"),
        );
        let mut finish = worker.finish();
        assert!(matches!(
            finish.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release open");
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
        drop_notice
            .try_recv()
            .expect("writer dropped before finish receipt");
    }

    #[test]
    fn panicked_writer_rejects_retry_after_another_writer_updates_disk() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "journal-panic-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("journal.json");
        let worker_path = path.clone();
        let (worker, hydration) = JournalWorker::start_with(
            move || MutationJournal::open(&worker_path),
            |_, _| panic!("injected worker failure"),
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        assert_eq!(
            receive(worker.submit(snapshot("unsaved first writer"))),
            Err(JournalFailure::WorkerStopped)
        );
        let (mut second, _) = MutationJournal::open(&path).expect("panic releases the lock");
        assert!(matches!(
            second.save(&snapshot("second writer")),
            Ok(JournalCommit::Durable)
        ));
        assert_eq!(
            receive(worker.submit(snapshot("stale retry"))),
            Err(JournalFailure::WorkerStopped)
        );
        drop(second);
        let (journal, loaded) = MutationJournal::open(&path).expect("read second writer's data");
        let LoadedJournal::Present(snapshot) = loaded else {
            panic!("second writer saved a snapshot");
        };
        assert_eq!(snapshot.drafts[0].1.text, "second writer");
        drop(journal);
        std::fs::remove_dir_all(directory).expect("remove test journal");
    }

    #[test]
    fn open_and_save_run_on_the_worker_thread() {
        let caller = thread::current().id();
        let (worker, hydration) = JournalWorker::start_with(
            move || {
                assert_ne!(
                    thread::current().id(),
                    caller,
                    "open must not perform caller-thread I/O"
                );
                Ok((thread::current().id(), LoadedJournal::Missing))
            },
            move |owner, _| {
                assert_ne!(
                    thread::current().id(),
                    caller,
                    "save must not perform caller-thread I/O"
                );
                assert_eq!(thread::current().id(), *owner, "one thread owns the writer");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        assert_eq!(
            receive(worker.submit(snapshot("text"))),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
    }
}
