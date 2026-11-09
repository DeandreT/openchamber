//! Captured knowledge writes, serialized independently of UI completions.

use std::future::Future;
use std::pin::Pin;

use openchamber_native_core::knowledge::{Note, Plan};
use openchamber_native_transport::client::ServerClient;
use tokio::sync::{mpsc, oneshot, watch};

use crate::network::{Network, NetworkTask};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DocumentKey {
    Note(String),
    Plan(String),
}

#[derive(Clone)]
pub(crate) enum SaveOutcome {
    Note(Note),
    Plan(Plan),
    Missing,
    Failed,
}

impl SaveOutcome {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(self, Self::Note(_) | Self::Plan(_))
    }
}

/// One captured save's result, shared by presentation and an accepted send.
#[derive(Clone)]
pub(crate) struct SaveReceipt(watch::Receiver<Option<SaveOutcome>>);

impl SaveReceipt {
    pub(crate) fn same_save(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }

    pub(crate) async fn outcome(mut self) -> SaveOutcome {
        loop {
            if let Some(outcome) = self.0.borrow().clone() {
                return outcome;
            }
            if self.0.changed().await.is_err() {
                return SaveOutcome::Failed;
            }
        }
    }
}

/// One entry's editor state; a pending request never changes its saved baseline.
pub(crate) struct DocumentDraft {
    pub(crate) text: String,
    pub(crate) saved: String,
    pub(crate) pending: Option<String>,
    pub(crate) pending_save: Option<SaveReceipt>,
    pub(crate) client: Option<ServerClient>,
    pub(crate) revision: u64,
    confirmed_save_revision: u64,
    pub(crate) missing: bool,
}

impl DocumentDraft {
    pub(crate) fn new(saved: String) -> Self {
        Self {
            text: saved.clone(),
            saved,
            pending: None,
            pending_save: None,
            client: None,
            revision: 0,
            confirmed_save_revision: 0,
            missing: false,
        }
    }

    pub(crate) fn dirty(&self) -> bool {
        self.text != self.saved
    }

    pub(crate) fn set_text(&mut self, text: String) {
        if self.text != text {
            self.revision += 1;
            self.text = text;
        }
    }

    pub(crate) fn begin(&mut self) -> Option<String> {
        if self.pending.is_some() || !self.dirty() {
            return None;
        }
        let text = self.text.clone();
        self.revision += 1;
        self.pending = Some(text.clone());
        Some(text)
    }

    /// Confirms successful writes in request order without changing pending custody.
    pub(crate) fn acknowledge_save(&mut self, revision: u64, sent: &str, outcome: &SaveOutcome) {
        if !outcome.succeeded() || revision <= self.confirmed_save_revision {
            return;
        }
        self.confirmed_save_revision = revision;
        self.saved = sent.to_owned();
        self.revision += 1;
    }

    /// Returns whether a later accepted edit should follow this request.
    pub(crate) fn finish(&mut self, sent: &str, outcome: &SaveOutcome) -> bool {
        if self.pending.as_deref() != Some(sent) {
            return false;
        }
        self.pending = None;
        self.pending_save = None;
        self.revision += 1;
        self.missing = matches!(outcome, SaveOutcome::Missing);
        if outcome.succeeded() {
            self.saved = sent.to_owned();
        }
        self.text != sent
    }
}

type Write = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The executor stays with accepted writes across host switches and app quit.
pub(crate) struct KnowledgeWriter {
    sender: Option<mpsc::UnboundedSender<Write>>,
    worker: Option<NetworkTask<()>>,
    network: Option<Network>,
}

impl KnowledgeWriter {
    pub(crate) fn new() -> std::io::Result<Self> {
        let network = Network::new()?;
        let (sender, mut requests) = mpsc::unbounded_channel::<Write>();
        let worker = network.run(async move {
            while let Some(write) = requests.recv().await {
                write.await;
            }
        });
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            network: Some(network),
        })
    }

    pub(crate) fn save(
        &self,
        client: ServerClient,
        project: String,
        key: DocumentKey,
        text: String,
    ) -> oneshot::Receiver<SaveOutcome> {
        self.run(async move { save(&client, &project, &key, &text).await })
    }

    pub(crate) fn save_observed(
        &self,
        client: ServerClient,
        project: String,
        key: DocumentKey,
        text: String,
    ) -> SaveReceipt {
        let (outcome, waiting) = watch::channel(None);
        drop(self.run(async move {
            let _ = outcome.send_replace(Some(save(&client, &project, &key, &text).await));
        }));
        SaveReceipt(waiting)
    }

    pub(crate) fn run<F>(&self, request: F) -> oneshot::Receiver<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (receipt, waiting) = oneshot::channel();
        if let Some(sender) = &self.sender {
            let _ = sender.send(Box::pin(async move {
                let _ = receipt.send(request.await);
            }));
        }
        waiting
    }

    /// Drains queued writes without waiting for a GPUI callback.
    pub(crate) async fn finish(mut self) {
        self.sender = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
        self.network = None;
    }
}

async fn save(client: &ServerClient, project: &str, key: &DocumentKey, text: &str) -> SaveOutcome {
    match key {
        DocumentKey::Note(id) => match client.update_note(project, id, text).await {
            Ok(Some(note)) if note.id == *id => SaveOutcome::Note(note),
            Ok(None) => SaveOutcome::Missing,
            _ => SaveOutcome::Failed,
        },
        DocumentKey::Plan(id) => match client.save_plan(project, id, text).await {
            Ok(Some((plan, raw))) if plan.id == *id && raw == text => SaveOutcome::Plan(plan),
            Ok(None) => SaveOutcome::Missing,
            _ => SaveOutcome::Failed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_edits_wait_for_the_original_write_and_survive_its_failure() {
        let mut draft = DocumentDraft::new("saved".into());
        draft.text = "first edit".into();
        assert_eq!(draft.begin().as_deref(), Some("first edit"));
        draft.text = "later edit".into();
        assert!(draft.begin().is_none(), "only one request may own an entry");
        assert!(draft.finish("first edit", &SaveOutcome::Failed));
        assert_eq!(draft.saved, "saved", "failure never confirms the sent text");
        assert_eq!(draft.text, "later edit", "new input stays intact");
        assert_eq!(draft.begin().as_deref(), Some("later edit"));
    }

    #[test]
    fn a_failed_unchanged_draft_waits_for_an_explicit_retry() {
        let mut draft = DocumentDraft::new("saved".into());
        draft.text = "unsaved".into();
        let sent = draft.begin().expect("dirty draft starts a write");
        assert!(!draft.finish(&sent, &SaveOutcome::Failed));
        assert!(draft.dirty(), "the failed draft remains recoverable");
        assert_eq!(draft.begin().as_deref(), Some("unsaved"));
    }

    #[tokio::test]
    async fn quit_drains_writes_in_submission_order_without_ui_receipts() {
        use crate::scheduled_tasks_tests::Scripted;
        use serde_json::json;

        let server = Scripted::new(|request| {
            (
                200,
                json!({"note": {
                    "id": "n1", "body": request.body["body"], "createdAt": 1,
                    "updatedAt": 2, "source": "manual"
                }}),
            )
        });
        let writer = KnowledgeWriter::new().expect("write executor starts");
        for text in ["first edit", "later edit"] {
            let receipt = writer.save(
                server.client(),
                "project".into(),
                DocumentKey::Note("n1".into()),
                text.into(),
            );
            drop(receipt);
        }
        writer.finish().await;
        let requests = server.requests("PATCH", "/api/project-context/project/notes/n1");
        assert_eq!(requests.len(), 2, "quit must retain both accepted writes");
        assert_eq!(requests[0].body["body"], json!("first edit"));
        assert_eq!(requests[1].body["body"], json!("later edit"));
    }

    fn successful_note(text: &str) -> SaveOutcome {
        SaveOutcome::Note(openchamber_native_core::knowledge::Note {
            id: "n1".into(),
            body: text.into(),
            created_at: 1,
            updated_at: 2,
            source: Default::default(),
            plan_id: None,
        })
    }

    #[test]
    fn successful_baselines_follow_request_order_without_releasing_newer_pending() {
        let mut draft = DocumentDraft::new("Original".into());
        draft.set_text("Newer B".into());
        draft.begin().expect("accepted B is pending");
        let revision = draft.revision;
        draft.acknowledge_save(1, "Earlier A", &successful_note("Earlier A"));
        assert_eq!(
            draft.saved, "Earlier A",
            "the earlier successful write is the server baseline"
        );
        assert_eq!(
            draft.pending.as_deref(),
            Some("Newer B"),
            "an earlier acknowledgment cannot release B"
        );
        assert_eq!(draft.text, "Newer B", "accepted editor text stays intact");
        assert!(
            draft.revision > revision,
            "confirmed baseline changes invalidate older reads"
        );
        draft.acknowledge_save(2, "Newer B", &SaveOutcome::Failed);
        draft.finish("Newer B", &SaveOutcome::Failed);
        assert_eq!(
            draft.saved, "Earlier A",
            "failed B cannot hide successful A"
        );
        draft.set_text("Original".into());
        assert!(
            draft.dirty(),
            "returning to the original text requires a restoring write"
        );
    }

    #[test]
    fn reordered_and_aba_success_receipts_cannot_roll_back_a_newer_baseline_or_clear_missing() {
        let mut draft = DocumentDraft::new("Original".into());
        draft.set_text("Pending C".into());
        draft.begin().expect("accepted C is pending");
        draft.missing = true;
        draft.acknowledge_save(2, "B", &successful_note("B"));
        let confirmed = draft.revision;
        draft.acknowledge_save(1, "A", &successful_note("A"));
        assert_eq!(
            draft.saved, "B",
            "a delayed earlier success cannot replace B"
        );
        assert_eq!(
            draft.revision, confirmed,
            "a stale success makes no authoritative change"
        );
        draft.acknowledge_save(3, "A", &successful_note("A"));
        draft.acknowledge_save(2, "B", &successful_note("B"));
        assert_eq!(
            draft.saved, "A",
            "the later A remains confirmed after an old B receipt"
        );
        assert_eq!(
            draft.pending.as_deref(),
            Some("Pending C"),
            "only its exact receipt can release pending custody"
        );
        assert!(
            draft.missing,
            "successful save acknowledgment cannot erase authoritative deletion recovery"
        );
    }
}
