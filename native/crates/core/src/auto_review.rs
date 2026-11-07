//! Auto-review runs as the composer shows them, ported from
//! `stores/useAutoReviewStore.ts`.
//!
//! The OpenChamber server owns every run (`web/server/lib/auto-review`) and
//! drives its loop whether or not a client is open. The client holds a
//! projection keyed by the run's owner (the host, or the isolated space whose
//! directory the run names) and the reviewed session. Each owner numbers its
//! own revisions, and the projection never goes back to an older one. Nothing
//! here is persisted.

use std::collections::HashMap;

use crate::ids::SessionId;
use crate::message_queue::queue_owner;

/// How a run stands: `Running`, or how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunStatus {
    Running,
    /// Nothing was left; the reviewer's last word reached the implementer.
    Clean,
    /// Only declines the reviewer still contests remain.
    Disputed,
    /// Five re-reviews without an end.
    Limit,
    Failed,
    /// The user aborted the turn the run waited on.
    Stopped,
}

impl RunStatus {
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "clean" => Self::Clean,
            "disputed" => Self::Disputed,
            "limit" => Self::Limit,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => return None,
        })
    }

    /// The result row's icon and label (`RESULT` in `AutoReviewStrip`);
    /// `None` while the run is running.
    pub fn result(self) -> Option<RunResult> {
        let (icon, tone, label_key) = match self {
            Self::Running => return None,
            Self::Clean => (
                "checkbox-circle",
                ResultTone::Success,
                "chat.autoReview.result.clean",
            ),
            Self::Disputed => (
                "error-warning",
                ResultTone::Warning,
                "chat.autoReview.result.disputed",
            ),
            Self::Limit => (
                "error-warning",
                ResultTone::Warning,
                "chat.autoReview.result.limit",
            ),
            Self::Failed => (
                "close-circle",
                ResultTone::Error,
                "chat.autoReview.result.failed",
            ),
            Self::Stopped => ("stop", ResultTone::Muted, "chat.autoReview.result.stopped"),
        };
        Some(RunResult {
            icon,
            tone,
            label_key,
        })
    }
}

/// How a settled run's row looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunResult {
    pub icon: &'static str,
    pub tone: ResultTone,
    pub label_key: &'static str,
}

/// The theme role of a result icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultTone {
    Success,
    Warning,
    Error,
    Muted,
}

/// What the run is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunStep {
    /// The implementer's handoff.
    Handoff,
    /// The reviewer's review.
    Review,
    /// The implementer's answer to the review.
    Implement,
}

impl RunStep {
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "handoff" => Self::Handoff,
            "review" => Self::Review,
            "implement" => Self::Implement,
            _ => return None,
        })
    }

    /// The running row's status (`AutoReviewStrip`).
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Review => "chat.autoReview.status.waitingForReviewer",
            Self::Handoff | Self::Implement => "chat.autoReview.status.waitingForImplementer",
        }
    }
}

/// One run as the server serves it, without its prompts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoReviewRun {
    pub original_session: SessionId,
    pub review_session: SessionId,
    pub directory: String,
    pub status: RunStatus,
    pub step: RunStep,
    pub round: u32,
    pub max_rounds: u32,
    pub started_at: i64,
    pub updated_at: i64,
    /// One line, at most 500 characters.
    pub error: Option<String>,
    /// Both positions of a disputed run, as the reviewer summed them up.
    pub dispute: Option<String>,
}

/// The server that owns a run of `directory`: the space's for a directory
/// inside one, else the host's (`ownerOf`).
pub fn owner_of(directory: &str) -> Option<&str> {
    queue_owner(directory)
}

type Owner = Option<String>;
type RunKey = (Owner, SessionId);

/// The runs of one runtime, by owner and reviewed session.
#[derive(Debug, Default)]
pub struct AutoReviewProjection {
    runs: HashMap<RunKey, AutoReviewRun>,
    /// The revision last applied per run.
    applied: HashMap<RunKey, u64>,
    /// The revision of each owner's last full snapshot.
    snapshots: HashMap<Owner, u64>,
}

impl AutoReviewProjection {
    /// The run of a chat, read under the owner of the chat's own directory.
    pub fn run(&self, session: &SessionId, directory: &str) -> Option<&AutoReviewRun> {
        self.runs
            .get(&(owner_of(directory).map(str::to_owned), session.clone()))
    }

    /// One run's authoritative state from `owner`. A space answers for
    /// itself only: whatever it names lands under its own owner, so it cannot
    /// put a run on a host chat. Returns whether anything changed.
    pub fn apply_update(
        &mut self,
        owner: Option<&str>,
        revision: u64,
        session: &SessionId,
        directory: &str,
        run: Option<AutoReviewRun>,
    ) -> bool {
        if owner_of(directory) != owner {
            return false;
        }
        if run.as_ref().is_some_and(|run| {
            &run.original_session != session || owner_of(&run.directory) != owner
        }) {
            return false;
        }
        let owner = owner.map(str::to_owned);
        if self
            .snapshots
            .get(&owner)
            .is_some_and(|snapshot| *snapshot > revision)
        {
            return false;
        }
        let key = (owner, session.clone());
        if self
            .applied
            .get(&key)
            .is_some_and(|applied| *applied > revision)
        {
            return false;
        }
        self.applied.insert(key.clone(), revision);
        match run {
            Some(run) => self.runs.insert(key, run.clone()).as_ref() != Some(&run),
            None => self.runs.remove(&key).is_some(),
        }
    }

    /// A full snapshot of one owner replaces that owner's runs, except those
    /// a change newer than the snapshot set.
    pub fn apply_snapshot(&mut self, owner: Option<&str>, revision: u64, runs: Vec<AutoReviewRun>) {
        let owner = owner.map(str::to_owned);
        if self
            .snapshots
            .get(&owner)
            .is_some_and(|snapshot| *snapshot > revision)
        {
            return;
        }
        self.snapshots.insert(owner.clone(), revision);
        let applied = &self.applied;
        self.runs.retain(|key, run| {
            owner_of(&run.directory) != owner.as_deref()
                || applied.get(key).is_some_and(|applied| *applied > revision)
        });
        for run in runs {
            if owner_of(&run.directory) != owner.as_deref() {
                continue;
            }
            let key = (owner.clone(), run.original_session.clone());
            if self
                .applied
                .get(&key)
                .is_some_and(|applied| *applied > revision)
            {
                continue;
            }
            self.applied.insert(key.clone(), revision);
            self.runs.insert(key, run);
        }
    }

    /// Removes a run locally ahead of the server's answer (`removeRun`),
    /// returning it so a failure can put it back.
    pub fn take(&mut self, session: &SessionId, directory: &str) -> Option<AutoReviewRun> {
        self.runs
            .remove(&(owner_of(directory).map(str::to_owned), session.clone()))
    }

    /// Puts back a run whose removal failed, unless something newer arrived.
    pub fn restore(&mut self, run: AutoReviewRun) {
        let key = (
            owner_of(&run.directory).map(str::to_owned),
            run.original_session.clone(),
        );
        self.runs.entry(key).or_insert(run);
    }

    /// A deleted session's run goes with it.
    pub fn forget_session(&mut self, session: &SessionId) {
        self.runs.retain(|(_, key), _| key != session);
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: &str = "/spaces/0123456789ab/repo";

    fn run(session: &str, directory: &str, status: RunStatus) -> AutoReviewRun {
        AutoReviewRun {
            original_session: SessionId::new(session),
            review_session: SessionId::new(format!("{session}_review")),
            directory: directory.to_owned(),
            status,
            step: RunStep::Review,
            round: 0,
            max_rounds: 5,
            started_at: 1,
            updated_at: 1,
            error: None,
            dispute: None,
        }
    }

    #[test]
    fn updates_never_go_back_to_an_older_revision() {
        let mut projection = AutoReviewProjection::default();
        let session = SessionId::new("ses_a");
        assert!(projection.apply_update(
            None,
            3,
            &session,
            "/repo",
            Some(run("ses_a", "/repo", RunStatus::Running))
        ));
        assert!(!projection.apply_update(None, 2, &session, "/repo", None));
        assert_eq!(
            projection.run(&session, "/repo").map(|run| run.status),
            Some(RunStatus::Running)
        );
        assert!(projection.apply_update(
            None,
            4,
            &session,
            "/repo",
            Some(run("ses_a", "/repo", RunStatus::Clean))
        ));
        assert!(
            !projection.apply_update(
                None,
                4,
                &session,
                "/repo",
                Some(run("ses_a", "/repo", RunStatus::Clean))
            ),
            "an identical update changes nothing"
        );
        assert!(projection.apply_update(None, 5, &session, "/repo", None));
        assert!(projection.run(&session, "/repo").is_none());
    }

    #[test]
    fn a_snapshot_keeps_changes_newer_than_itself() {
        let mut projection = AutoReviewProjection::default();
        let newer = SessionId::new("ses_new");
        projection.apply_update(
            None,
            9,
            &newer,
            "/repo",
            Some(run("ses_new", "/repo", RunStatus::Running)),
        );
        projection.apply_snapshot(
            None,
            7,
            vec![
                run("ses_old", "/repo", RunStatus::Failed),
                run("ses_new", "/repo", RunStatus::Clean),
            ],
        );
        assert_eq!(
            projection.run(&newer, "/repo").map(|run| run.status),
            Some(RunStatus::Running)
        );
        assert!(
            projection
                .run(&SessionId::new("ses_old"), "/repo")
                .is_some()
        );
        projection.apply_snapshot(None, 10, Vec::new());
        assert!(
            projection.is_empty(),
            "a later empty snapshot clears the owner"
        );
        projection.apply_snapshot(None, 8, vec![run("ses_old", "/repo", RunStatus::Failed)]);
        assert!(projection.is_empty(), "an older snapshot is ignored");
    }

    #[test]
    fn a_space_cannot_put_a_run_on_a_host_chat() {
        let mut projection = AutoReviewProjection::default();
        let session = SessionId::new("ses_a");
        assert!(!projection.apply_update(
            Some("0123456789ab"),
            1,
            &session,
            "/repo",
            Some(run("ses_a", "/repo", RunStatus::Running))
        ));
        assert!(projection.apply_update(
            Some("0123456789ab"),
            1,
            &session,
            SPACE,
            Some(run("ses_a", SPACE, RunStatus::Running))
        ));
        assert!(projection.run(&session, "/repo").is_none());
        assert!(projection.run(&session, SPACE).is_some());
        // A snapshot of the host leaves the space's runs alone.
        projection.apply_snapshot(None, 5, Vec::new());
        assert!(projection.run(&session, SPACE).is_some());
    }

    #[test]
    fn a_failed_removal_puts_the_run_back() {
        let mut projection = AutoReviewProjection::default();
        let session = SessionId::new("ses_a");
        projection.apply_update(
            None,
            1,
            &session,
            "/repo",
            Some(run("ses_a", "/repo", RunStatus::Running)),
        );
        let taken = projection.take(&session, "/repo").expect("run");
        assert!(projection.run(&session, "/repo").is_none());
        projection.restore(taken);
        assert!(projection.run(&session, "/repo").is_some());
    }

    #[test]
    fn results_follow_the_reference_rows() {
        assert_eq!(RunStatus::Running.result(), None);
        assert_eq!(
            RunStatus::Clean.result().map(|result| result.label_key),
            Some("chat.autoReview.result.clean")
        );
        assert_eq!(
            RunStatus::Stopped.result().map(|result| result.tone),
            Some(ResultTone::Muted)
        );
        assert_eq!(
            RunStep::Review.label_key(),
            "chat.autoReview.status.waitingForReviewer"
        );
        assert_eq!(
            RunStep::Handoff.label_key(),
            "chat.autoReview.status.waitingForImplementer"
        );
    }
}
