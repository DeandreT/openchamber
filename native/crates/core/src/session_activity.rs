//! What the live channel and the host's status snapshot established about
//! each session's turns, beyond the running state itself: how the last turn
//! ended, when the running one began, and whether the snapshot may speak for
//! a session it does not list.
//!
//! Mirrors the parts of `sync/global-session-status.ts` (`observedById`),
//! `sync/session-activity-timing.ts` (`startedAt`) and the directory store's
//! `sessionStatusReady` and `sessionStatusInvalidated` that the work status
//! card reads. Running state stays with its own owner; this module never
//! answers whether a session is busy.
//!
//! Nothing here is persisted. A runtime switch resets it, and a reread after
//! a new connection or missed events withdraws the snapshot's coverage until
//! that reread's own snapshot applies.

use std::collections::{HashMap, HashSet};

use crate::ids::SessionId;
use crate::session::LiveStatus;

/// How a session's last observed turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnOutcome {
    /// The turn ended without a reported failure.
    Settled,
    /// The turn ended with an error. A later idle report for the same turn
    /// keeps this outcome; the next running report clears it.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observation {
    Running,
    Ended(TurnOutcome),
}

/// Turn observations for the sessions of one runtime.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionActivity {
    observed: HashMap<SessionId, Observation>,
    /// When each running turn began, in this device's clock.
    started: HashMap<SessionId, i64>,
    /// A status snapshot is applied and nothing since has made it stale.
    snapshot_ready: bool,
    /// Sessions the applied snapshot cannot certify idle.
    invalidated: HashSet<SessionId>,
}

impl SessionActivity {
    /// Creates an owner that has observed nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one live status report. A running report says the session is
    /// still working, never that a turn began.
    pub fn observe_status(&mut self, id: &SessionId, status: LiveStatus) {
        self.invalidated.remove(id);
        match status {
            LiveStatus::Busy | LiveStatus::Retry => {
                self.observed.insert(id.clone(), Observation::Running);
            }
            LiveStatus::Idle => {
                self.started.remove(id);
                if self.outcome(id) != Some(TurnOutcome::Failed) {
                    self.observed
                        .insert(id.clone(), Observation::Ended(TurnOutcome::Settled));
                }
            }
        }
    }

    /// Records that this device saw a turn begin at `at`, in its own clock.
    pub fn turn_started(&mut self, id: &SessionId, at: i64) {
        self.invalidated.remove(id);
        self.observed.insert(id.clone(), Observation::Running);
        self.started.insert(id.clone(), at);
    }

    /// Records that the session's turn ended with an error.
    pub fn turn_failed(&mut self, id: &SessionId) {
        self.invalidated.remove(id);
        self.started.remove(id);
        self.observed
            .insert(id.clone(), Observation::Ended(TurnOutcome::Failed));
    }

    /// Applies a status snapshot that lists `running` and the host's start
    /// for each running turn it knows, already moved onto this device's
    /// clock.
    ///
    /// A session observed running that the snapshot does not list loses that
    /// observation and its start: whether it is idle is then the snapshot's
    /// own answer, through [`Self::snapshot_covers`]. Ended turns keep their
    /// outcome. A host start fills a missing start and replaces a later one,
    /// since this device stamps a start when the event arrives.
    pub fn apply_snapshot(
        &mut self,
        running: &HashSet<SessionId>,
        host_starts: &[(SessionId, i64)],
    ) {
        self.snapshot_ready = true;
        self.invalidated.clear();
        self.observed
            .retain(|id, observation| *observation != Observation::Running || running.contains(id));
        self.started.retain(|id, _| running.contains(id));
        for id in running {
            self.observed.insert(id.clone(), Observation::Running);
        }
        for (id, at) in host_starts {
            if !running.contains(id) {
                continue;
            }
            let start = self.started.entry(id.clone()).or_insert(*at);
            *start = (*start).min(*at);
        }
    }

    /// Withdraws the applied snapshot's coverage of every session: events
    /// may have been missed since it was read, or the read meant to replace
    /// it failed. Observed outcomes and starts stay, since each came from an
    /// explicit report; only "not listed, so idle" stops being an answer
    /// until the next snapshot applies.
    pub fn withdraw_snapshot(&mut self) {
        self.snapshot_ready = false;
    }

    /// Marks a session whose status the applied snapshot no longer speaks
    /// for, as archiving does: only a later status report or snapshot can
    /// certify it idle.
    pub fn invalidate(&mut self, id: &SessionId) {
        self.observed.remove(id);
        self.started.remove(id);
        self.invalidated.insert(id.clone());
    }

    /// Forgets a deleted session.
    pub fn forget(&mut self, id: &SessionId) {
        self.observed.remove(id);
        self.started.remove(id);
        self.invalidated.remove(id);
    }

    /// How the session's last observed turn ended. `None` while it runs and
    /// for a session this runtime never saw report a status.
    pub fn outcome(&self, id: &SessionId) -> Option<TurnOutcome> {
        match self.observed.get(id)? {
            Observation::Running => None,
            Observation::Ended(outcome) => Some(*outcome),
        }
    }

    /// When the session's running turn began, in this device's clock. `None`
    /// when no start is known: a running session then shows no duration.
    pub fn started_at(&self, id: &SessionId) -> Option<i64> {
        self.started.get(id).copied()
    }

    /// Whether the applied snapshot may certify `id` idle by not listing it.
    /// The caller still checks that the session lies inside the boundary the
    /// snapshot was read from.
    pub fn snapshot_covers(&self, id: &SessionId) -> bool {
        self.snapshot_ready && !self.invalidated.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> SessionId {
        SessionId::from(value)
    }

    fn running(ids: &[&str]) -> HashSet<SessionId> {
        ids.iter().map(|value| id(value)).collect()
    }

    #[test]
    fn a_failure_survives_the_idle_report_of_its_turn_and_ends_at_the_next_run() {
        let mut activity = SessionActivity::new();
        let child = id("ses_child");
        activity.turn_started(&child, 100);
        assert_eq!(activity.outcome(&child), None);
        activity.turn_failed(&child);
        activity.observe_status(&child, LiveStatus::Idle);
        assert_eq!(activity.outcome(&child), Some(TurnOutcome::Failed));
        assert_eq!(activity.started_at(&child), None);
        activity.observe_status(&child, LiveStatus::Busy);
        assert_eq!(activity.outcome(&child), None);
        activity.observe_status(&child, LiveStatus::Idle);
        assert_eq!(activity.outcome(&child), Some(TurnOutcome::Settled));
    }

    #[test]
    fn a_running_report_is_not_a_turn_start() {
        let mut activity = SessionActivity::new();
        let child = id("ses_child");
        activity.observe_status(&child, LiveStatus::Busy);
        assert_eq!(activity.started_at(&child), None);
        activity.turn_started(&child, 100);
        activity.observe_status(&child, LiveStatus::Retry);
        assert_eq!(activity.started_at(&child), Some(100));
    }

    #[test]
    fn no_snapshot_means_no_coverage() {
        let mut activity = SessionActivity::new();
        let child = id("ses_child");
        assert!(!activity.snapshot_covers(&child));
        activity.apply_snapshot(&running(&[]), &[]);
        assert!(activity.snapshot_covers(&child));
    }

    #[test]
    fn a_withdrawn_snapshot_covers_nothing_until_the_next_one_applies() {
        let mut activity = SessionActivity::new();
        let quiet = id("ses_quiet");
        let settled = id("ses_settled");
        let failed = id("ses_failed");
        let live = id("ses_live");
        activity.apply_snapshot(&running(&["ses_live"]), &[(live.clone(), 40)]);
        activity.observe_status(&settled, LiveStatus::Idle);
        activity.turn_failed(&failed);
        assert!(activity.snapshot_covers(&quiet));

        activity.withdraw_snapshot();
        assert!(!activity.snapshot_covers(&quiet));
        assert!(!activity.snapshot_covers(&settled));
        assert_eq!(activity.outcome(&settled), Some(TurnOutcome::Settled));
        assert_eq!(activity.outcome(&failed), Some(TurnOutcome::Failed));
        assert_eq!(activity.started_at(&live), Some(40));

        // A status report certifies its own session, never the snapshot.
        activity.observe_status(&live, LiveStatus::Idle);
        assert_eq!(activity.outcome(&live), Some(TurnOutcome::Settled));
        assert!(!activity.snapshot_covers(&quiet));

        activity.apply_snapshot(&running(&[]), &[]);
        assert!(activity.snapshot_covers(&quiet));
        assert_eq!(activity.outcome(&failed), Some(TurnOutcome::Failed));
    }

    #[test]
    fn an_invalidated_session_needs_a_fresh_report() {
        let mut activity = SessionActivity::new();
        let archived = id("ses_archived");
        let other = id("ses_other");
        activity.apply_snapshot(&running(&[]), &[]);
        activity.observe_status(&archived, LiveStatus::Idle);
        activity.invalidate(&archived);
        assert!(!activity.snapshot_covers(&archived));
        assert_eq!(activity.outcome(&archived), None);
        assert!(activity.snapshot_covers(&other), "one session only");
        activity.observe_status(&archived, LiveStatus::Idle);
        assert!(activity.snapshot_covers(&archived));

        activity.invalidate(&archived);
        activity.apply_snapshot(&running(&[]), &[]);
        assert!(
            activity.snapshot_covers(&archived),
            "a newer snapshot covers it"
        );
    }

    #[test]
    fn a_snapshot_drops_stale_running_observations_and_keeps_outcomes() {
        let mut activity = SessionActivity::new();
        let stale = id("ses_stale");
        let failed = id("ses_failed");
        let live = id("ses_live");
        activity.turn_started(&stale, 10);
        activity.turn_failed(&failed);
        activity.turn_started(&live, 500);
        activity.apply_snapshot(
            &running(&["ses_live", "ses_joined"]),
            &[
                (live.clone(), 400),
                (id("ses_joined"), 50),
                (stale.clone(), 5),
            ],
        );
        assert_eq!(activity.outcome(&stale), None);
        assert_eq!(activity.started_at(&stale), None, "not running any more");
        assert_eq!(activity.outcome(&failed), Some(TurnOutcome::Failed));
        assert_eq!(
            activity.started_at(&live),
            Some(400),
            "the host saw it first"
        );
        assert_eq!(activity.started_at(&id("ses_joined")), Some(50));

        activity.apply_snapshot(&running(&["ses_live"]), &[(live.clone(), 450)]);
        assert_eq!(
            activity.started_at(&live),
            Some(400),
            "a later start is not news"
        );
    }

    #[test]
    fn forgetting_one_session_keeps_the_others() {
        let mut activity = SessionActivity::new();
        let gone = id("ses_gone");
        let kept = id("ses_kept");
        activity.turn_started(&gone, 1);
        activity.turn_failed(&kept);
        activity.forget(&gone);
        assert_eq!(activity.started_at(&gone), None);
        assert_eq!(activity.outcome(&kept), Some(TurnOutcome::Failed));
    }
}
