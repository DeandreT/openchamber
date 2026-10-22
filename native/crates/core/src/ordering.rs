//! Lifecycle ordering of sessions in sidebar lists.
//!
//! Mirrors `packages/ui/src/sync/session-ordering.ts`. A session's position
//! comes from a frozen baseline (its last conversation time when first seen)
//! until a live activity transition promotes it. Token arrivals and metadata
//! writes never reorder a list on their own.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::ids::SessionId;
use crate::session::SessionRecord;

/// Whether a session is currently running a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivityPhase {
    Active,
    Settled,
}

/// A change to lifecycle ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrderingMutation {
    /// A live channel reported the session's phase.
    Observe {
        session_id: SessionId,
        phase: ActivityPhase,
    },
    /// The session left every list.
    Remove { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Default)]
struct Baseline {
    created: Option<i64>,
    updated: Option<i64>,
}

/// Live ranks and frozen baselines for one runtime.
#[derive(Clone, Debug, Default)]
pub struct SessionOrdering {
    rank_by_id: HashMap<SessionId, i64>,
    phase_by_id: HashMap<SessionId, ActivityPhase>,
    baseline_by_id: HashMap<SessionId, Baseline>,
    last_rank: i64,
}

impl SessionOrdering {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies live phase observations; `now_ms` is the wall clock.
    ///
    /// A first observation that a session is settled does not promote it.
    /// Returns whether any rank changed.
    pub fn apply(&mut self, mutations: &[OrderingMutation], now_ms: i64) -> bool {
        let mut changed = false;
        for mutation in mutations {
            match mutation {
                OrderingMutation::Remove { session_id } => {
                    self.phase_by_id.remove(session_id);
                    self.baseline_by_id.remove(session_id);
                    changed |= self.rank_by_id.remove(session_id).is_some();
                }
                OrderingMutation::Observe { session_id, phase } => {
                    let previous = self.phase_by_id.insert(session_id.clone(), *phase);
                    if previous == Some(*phase)
                        || (previous.is_none() && *phase == ActivityPhase::Settled)
                    {
                        continue;
                    }
                    let rank = self.next_rank(now_ms);
                    self.rank_by_id.insert(session_id.clone(), rank);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Reconciles phases from a status snapshot taken after missed events.
    ///
    /// A snapshot cannot recover the order of missed transitions, so every
    /// changed session shares one rank and timestamps break ties.
    pub fn reconcile_snapshot<'a>(
        &mut self,
        active: impl IntoIterator<Item = &'a SessionId>,
        known: impl IntoIterator<Item = &'a SessionId>,
        now_ms: i64,
    ) {
        let active: Vec<&SessionId> = active.into_iter().collect();
        let mut observed: Vec<&SessionId> = known.into_iter().collect();
        observed.extend(active.iter().copied());
        let mut promoted: Vec<SessionId> = Vec::new();
        for session_id in observed {
            let phase = if active.contains(&session_id) {
                ActivityPhase::Active
            } else {
                ActivityPhase::Settled
            };
            let previous = self.phase_by_id.insert(session_id.clone(), phase);
            if previous.is_some_and(|previous| previous != phase) && !promoted.contains(session_id)
            {
                promoted.push(session_id.clone());
            }
        }
        if promoted.is_empty() {
            return;
        }
        let rank = self.next_rank(now_ms);
        for session_id in promoted {
            self.rank_by_id.insert(session_id, rank);
        }
    }

    /// Raises baselines and stale live ranks to newer authoritative timestamps.
    ///
    /// Call when a global snapshot arrives. The change is monotonic, so it
    /// never demotes a session.
    pub fn raise_baselines<'a>(&mut self, sessions: impl IntoIterator<Item = &'a SessionRecord>) {
        for session in sessions {
            let fresh = activity_time(session);
            if let Some(rank) = self.rank_by_id.get_mut(&session.id) {
                if fresh > *rank {
                    *rank = fresh;
                }
                continue;
            }
            let baseline = self.baseline_by_id.entry(session.id.clone()).or_default();
            if baseline.updated.is_none_or(|updated| updated < fresh) {
                baseline.updated = Some(fresh);
            }
        }
    }

    /// The value a session sorts by: its live rank, else its baseline.
    pub fn order_value(&mut self, session: &SessionRecord, pinned: bool) -> i64 {
        match self.rank_by_id.get(&session.id) {
            Some(rank) => *rank,
            None => self.baseline(session, pinned),
        }
    }

    /// Orders two sessions: pinned first, then newest lifecycle first.
    ///
    /// Siblings compare live ranks; everything else compares frozen
    /// baselines, then creation time, then ID.
    pub fn compare(
        &mut self,
        left: &SessionRecord,
        right: &SessionRecord,
        left_pinned: bool,
        right_pinned: bool,
    ) -> Ordering {
        if left_pinned != right_pinned {
            return if left_pinned {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let left_fallback = self.baseline(left, left_pinned);
        let right_fallback = self.baseline(right, right_pinned);
        if left.parent_id == right.parent_id {
            let left_value = self.order_value(left, left_pinned);
            let right_value = self.order_value(right, right_pinned);
            let by_rank = right_value.cmp(&left_value);
            if by_rank.is_ne() {
                return by_rank;
            }
        }
        right_fallback
            .cmp(&left_fallback)
            .then_with(|| self.baseline(right, true).cmp(&self.baseline(left, true)))
            .then_with(|| left.id.cmp(&right.id))
    }

    fn baseline(&mut self, session: &SessionRecord, pinned: bool) -> i64 {
        let baseline = self.baseline_by_id.entry(session.id.clone()).or_default();
        let slot = if pinned {
            &mut baseline.created
        } else {
            &mut baseline.updated
        };
        *slot.get_or_insert_with(|| {
            if pinned {
                session.time.created
            } else {
                activity_time(session)
            }
        })
    }

    fn next_rank(&mut self, now_ms: i64) -> i64 {
        self.last_rank = (self.last_rank + 1).max(now_ms);
        self.last_rank
    }
}

/// When the session last had a conversation.
///
/// `idle` moves only when a turn ends; `updated` also moves on title and
/// metadata writes, so it is only a fallback for sessions that never idled.
pub fn activity_time(session: &SessionRecord) -> i64 {
    [
        session.time.idle,
        Some(session.time.updated),
        Some(session.time.created),
    ]
    .into_iter()
    .flatten()
    .find(|time| *time != 0)
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, created: i64, updated: i64) -> SessionRecord {
        let mut session = SessionRecord::new(id, "/p");
        session.time.created = created;
        session.time.updated = updated;
        session
    }

    #[test]
    fn newest_activity_sorts_first_and_pins_lead() {
        let mut ordering = SessionOrdering::new();
        let old = session("old", 1, 10);
        let new = session("new", 2, 20);
        assert_eq!(ordering.compare(&new, &old, false, false), Ordering::Less);
        assert_eq!(ordering.compare(&new, &old, false, true), Ordering::Greater);
    }

    #[test]
    fn baselines_freeze_until_a_transition_promotes() {
        let mut ordering = SessionOrdering::new();
        let a = session("a", 1, 10);
        let mut b = session("b", 1, 20);
        assert_eq!(ordering.compare(&b, &a, false, false), Ordering::Less);
        // A metadata write moves `updated` but not the frozen baseline.
        b.time.updated = 5;
        assert_eq!(ordering.compare(&b, &a, false, false), Ordering::Less);
        // A live transition promotes `a` above `b`.
        let observed = ordering.apply(
            &[OrderingMutation::Observe {
                session_id: SessionId::from("a"),
                phase: ActivityPhase::Active,
            }],
            1_000,
        );
        assert!(observed);
        assert_eq!(ordering.compare(&a, &b, false, false), Ordering::Less);
    }

    #[test]
    fn a_first_settled_observation_does_not_promote() {
        let mut ordering = SessionOrdering::new();
        let changed = ordering.apply(
            &[OrderingMutation::Observe {
                session_id: SessionId::from("a"),
                phase: ActivityPhase::Settled,
            }],
            1_000,
        );
        assert!(!changed);
    }

    #[test]
    fn idle_time_wins_over_updated() {
        let mut record = session("a", 1, 50);
        record.time.idle = Some(30);
        assert_eq!(activity_time(&record), 30);
    }
}
