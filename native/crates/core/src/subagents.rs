//! The work status card's Subagents rows
//! (`WorkStatusSubagentsSection.tsx`, `subagentCost.ts` and
//! `useSubagentCostRollup.ts`): which child sessions the section lists, what
//! each one is doing, their order and what each subtree has cost.
//!
//! Everything is derived from live session records and the caller's live
//! facts. Nothing here reads a transcript or history.

use std::collections::{HashMap, HashSet};

use crate::ids::SessionId;
use crate::permission_mode;
use crate::session::{LiveStatus, SessionRecord};
use crate::session_activity::TurnOutcome;
use crate::transcript::Usd;

/// What one child session is doing, in the section's precedence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubagentState {
    /// A permission request waits on the user.
    NeedsPermission,
    /// A question waits on the user.
    AskedQuestion,
    /// The child's turn runs or retries.
    Working,
    /// The child's last turn ended with an error.
    Failed,
    /// The child is known to be idle.
    Done,
    /// Nothing authoritative says whether the child runs.
    Unknown,
}

impl SubagentState {
    /// Blockers first, then running rows, then rows whose status is not
    /// known, then finished ones: an unknown row never pushes a working one
    /// down.
    fn rank(self) -> u8 {
        match self {
            Self::NeedsPermission | Self::AskedQuestion => 0,
            Self::Working => 1,
            Self::Unknown => 2,
            Self::Failed | Self::Done => 3,
        }
    }
}

/// The live facts about one child session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChildFacts {
    /// A permission request of the child is pending.
    pub permission: bool,
    /// A question of the child is pending.
    pub question: bool,
    /// The child's running state, when the live channel or a snapshot
    /// reports one.
    pub live: Option<LiveStatus>,
    /// How the child's last observed turn ended.
    pub outcome: Option<TurnOutcome>,
    /// A status snapshot of the boundary the child lies in is applied and
    /// still speaks for the child.
    pub snapshot_covers: bool,
}

impl ChildFacts {
    /// Whether the child's turn runs or retries, whatever else waits on it.
    pub fn busy(&self) -> bool {
        matches!(self.live, Some(LiveStatus::Busy | LiveStatus::Retry))
    }

    /// The row's state. Not being reported busy means done only with an
    /// observed end or a covering snapshot; otherwise the state is unknown.
    pub fn state(&self) -> SubagentState {
        if self.permission {
            SubagentState::NeedsPermission
        } else if self.question {
            SubagentState::AskedQuestion
        } else if self.busy() {
            SubagentState::Working
        } else if self.outcome == Some(TurnOutcome::Failed) {
            SubagentState::Failed
        } else if self.live == Some(LiveStatus::Idle)
            || self.outcome == Some(TurnOutcome::Settled)
            || self.snapshot_covers
        {
            SubagentState::Done
        } else {
            SubagentState::Unknown
        }
    }
}

/// One row of the section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentRow<'a> {
    pub session: &'a SessionRecord,
    pub state: SubagentState,
    /// The child's turn runs, also while a request of it waits on the user.
    pub busy: bool,
    /// The child's own cost plus every descendant's, when it is positive.
    pub cost: Option<Usd>,
}

/// The boundary a status snapshot is read from: one isolated space, or the
/// host for every other directory.
fn status_boundary(session: &SessionRecord) -> Option<Option<String>> {
    let directory = session.resolved_directory()?;
    Some(
        permission_mode::is_space_directory(&directory)
            .then(|| directory["/spaces/".len().."/spaces/".len() + 12].to_owned()),
    )
}

/// Whether the host's status snapshot speaks for `child` in a panel showing
/// `parent`: both lie in host directories. A space reads its own snapshot,
/// so the host's never covers a session inside one, and a session without a
/// directory is covered by nothing.
pub fn host_snapshot_boundary(parent: &SessionRecord, child: &SessionRecord) -> bool {
    status_boundary(parent) == Some(None) && status_boundary(child) == Some(None)
}

/// The rows for the children of `parent` among `sessions`, the live session
/// list: every record whose parent is `parent`, without workflow agents and
/// the sessions in `workflow_agents`, which their run lists.
///
/// Blockers come first, then running rows, unknown rows and finished rows;
/// inside each group the newest creation leads. Activity never reorders a
/// group, so a finished list reads as it did at launch.
pub fn rows<'a>(
    sessions: &'a [SessionRecord],
    parent: &SessionId,
    workflow_agents: &HashSet<SessionId>,
    facts: impl Fn(&SessionRecord) -> ChildFacts,
) -> Vec<SubagentRow<'a>> {
    let children: Vec<&SessionRecord> = sessions
        .iter()
        .filter(|session| {
            session.parent_id.as_ref() == Some(parent)
                && !session.is_workflow_agent()
                && !workflow_agents.contains(&session.id)
        })
        .collect();
    if children.is_empty() {
        return Vec::new();
    }
    let by_parent = children_by_parent(sessions);
    let mut rows: Vec<SubagentRow<'a>> = children
        .into_iter()
        .map(|session| {
            let child = facts(session);
            SubagentRow {
                session,
                state: child.state(),
                busy: child.busy(),
                cost: Some(subtree_cost(session, &by_parent)).filter(|cost| *cost > Usd::default()),
            }
        })
        .collect();
    // Stable, so equal creation times keep the list's own order.
    rows.sort_by(|left, right| {
        left.state
            .rank()
            .cmp(&right.state.rank())
            .then(right.session.time.created.cmp(&left.session.time.created))
    });
    rows
}

/// The heading's summary: `running/total` while any child runs, else the
/// total.
pub fn summary(rows: &[SubagentRow<'_>]) -> String {
    let busy = rows.iter().filter(|row| row.busy).count();
    if busy > 0 {
        format!("{busy}/{}", rows.len())
    } else {
        rows.len().to_string()
    }
}

fn children_by_parent(sessions: &[SessionRecord]) -> HashMap<&SessionId, Vec<&SessionRecord>> {
    let mut index: HashMap<&SessionId, Vec<&SessionRecord>> = HashMap::new();
    for session in sessions {
        if let Some(parent) = session
            .parent_id
            .as_ref()
            .filter(|id| !id.as_str().is_empty())
        {
            index.entry(parent).or_default().push(session);
        }
    }
    index
}

/// `computeSubtreeCost`: the session's own cost plus every descendant's.
/// Parent links should form a tree; a visited set keeps a cycle from being
/// walked twice.
fn subtree_cost(root: &SessionRecord, by_parent: &HashMap<&SessionId, Vec<&SessionRecord>>) -> Usd {
    let mut total = Usd::default();
    let mut visited: HashSet<&SessionId> = HashSet::new();
    let mut pending = vec![root];
    while let Some(session) = pending.pop() {
        if !visited.insert(&session.id) {
            continue;
        }
        total = total + session.cost.unwrap_or_default();
        if let Some(children) = by_parent.get(&session.id) {
            pending.extend(children.iter().copied());
        }
    }
    total
}

/// `formatCost`: dollars to four decimals without trailing zeros, rounded
/// as `Number.prototype.toFixed(4)` rounds the same `f64`.
pub fn format_cost(cost: Usd) -> String {
    let fixed = to_fixed_4(cost.dollars());
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    format!("${trimmed}")
}

/// `toFixed(4)` of a finite, non-negative `f64`: the four-decimal number
/// nearest to the value the binary float exactly holds, and the larger one
/// when two are equally near.
///
/// Rust's `{:.4}` also rounds the exact binary value, so the two agree
/// everywhere except on an exact tie, which Rust sends to the even digit.
/// A value such as `12.34565` is no tie: its `f64` lies just under the
/// decimal midpoint, and both print `12.3456`.
fn to_fixed_4(value: f64) -> String {
    match exact_tie_at_4(value) {
        // `doubled` is the odd integer `value * 20000`; the larger
        // neighbour of `value * 10000` is half of the next even one.
        Some(doubled) => {
            let ten_thousandths = doubled.div_ceil(2);
            format!(
                "{}.{:04}",
                ten_thousandths / 10_000,
                ten_thousandths % 10_000
            )
        }
        None => format!("{value:.4}"),
    }
}

/// `value * 20000` when the float holds exactly an odd multiple of
/// `0.00005`, the midpoint between two four-decimal numbers. Decided on the
/// float's own mantissa and exponent, so no rounding enters the test.
fn exact_tie_at_4(value: f64) -> Option<u128> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let bits = value.to_bits();
    let biased = (bits >> 52) & 0x7ff;
    let fraction = bits & ((1 << 52) - 1);
    // value == mantissa * 2^exponent.
    let (mantissa, exponent) = if biased == 0 {
        (fraction, -1074_i64)
    } else {
        (fraction | (1 << 52), biased as i64 - 1075)
    };
    // An integer times 20000 is even.
    if exponent >= 0 {
        return None;
    }
    // `mantissa * 20000` is under 2^68; a larger divisor leaves a fraction.
    let shift = u32::try_from(-exponent).ok().filter(|shift| *shift < 68)?;
    let scaled = u128::from(mantissa) * 20_000;
    let doubled = scaled >> shift;
    (scaled == doubled << shift && doubled % 2 == 1).then_some(doubled)
}

/// `formatSessionActivityDuration`'s parts: whole seconds under a minute,
/// minutes and seconds under an hour, hours and minutes after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityDuration {
    Seconds(i64),
    MinutesSeconds { minutes: i64, seconds: i64 },
    HoursMinutes { hours: i64, minutes: i64 },
}

impl ActivityDuration {
    /// The duration of a turn that began at `started_at`, as of `now`, both
    /// in milliseconds. A start in the future reads as zero.
    pub fn since(started_at: i64, now: i64) -> Self {
        let total = now.saturating_sub(started_at).max(0);
        if total < 60_000 {
            Self::Seconds(total / 1000)
        } else if total < 3_600_000 {
            Self::MinutesSeconds {
                minutes: total / 60_000,
                seconds: total % 60_000 / 1000,
            }
        } else {
            Self::HoursMinutes {
                hours: total / 3_600_000,
                minutes: total % 3_600_000 / 60_000,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionKind, SessionTime};

    const PARENT: &str = "ses_parent";

    fn child(id: &str, created: i64) -> SessionRecord {
        SessionRecord {
            parent_id: Some(SessionId::from(PARENT)),
            time: SessionTime {
                created,
                updated: created,
                idle: None,
                archived: None,
            },
            ..SessionRecord::new(id, "/repo")
        }
    }

    fn with_parent(mut session: SessionRecord, parent: &str) -> SessionRecord {
        session.parent_id = Some(SessionId::from(parent));
        session
    }

    fn with_cost(mut session: SessionRecord, dollars: f64) -> SessionRecord {
        session.cost = Usd::from_dollars(dollars);
        session
    }

    fn listed<'a>(
        sessions: &'a [SessionRecord],
        facts: impl Fn(&SessionRecord) -> ChildFacts,
    ) -> Vec<SubagentRow<'a>> {
        rows(sessions, &SessionId::from(PARENT), &HashSet::new(), facts)
    }

    fn ids<'a>(rows: &'a [SubagentRow<'_>]) -> Vec<&'a str> {
        rows.iter().map(|row| row.session.id.as_str()).collect()
    }

    const BUSY: ChildFacts = ChildFacts {
        permission: false,
        question: false,
        live: Some(LiveStatus::Busy),
        outcome: None,
        snapshot_covers: false,
    };

    #[test]
    fn state_follows_the_reference_precedence() {
        let everything = ChildFacts {
            permission: true,
            question: true,
            live: Some(LiveStatus::Busy),
            outcome: Some(TurnOutcome::Failed),
            snapshot_covers: true,
        };
        assert_eq!(everything.state(), SubagentState::NeedsPermission);
        assert!(everything.busy(), "a blocked child still counts as running");
        let asked = ChildFacts {
            permission: false,
            ..everything
        };
        assert_eq!(asked.state(), SubagentState::AskedQuestion);
        let working = ChildFacts {
            question: false,
            ..asked
        };
        assert_eq!(working.state(), SubagentState::Working);
        let retrying = ChildFacts {
            live: Some(LiveStatus::Retry),
            ..working
        };
        assert_eq!(retrying.state(), SubagentState::Working);
        let failed = ChildFacts {
            live: None,
            ..working
        };
        assert_eq!(failed.state(), SubagentState::Failed);
        let failed_then_idle = ChildFacts {
            live: Some(LiveStatus::Idle),
            ..failed
        };
        assert_eq!(failed_then_idle.state(), SubagentState::Failed);
        let done = ChildFacts {
            outcome: None,
            ..failed
        };
        assert_eq!(done.state(), SubagentState::Done);
    }

    #[test]
    fn not_busy_is_done_only_with_an_observed_end_or_a_covering_snapshot() {
        let silent = ChildFacts::default();
        assert_eq!(silent.state(), SubagentState::Unknown);
        assert_eq!(
            ChildFacts {
                snapshot_covers: true,
                ..silent
            }
            .state(),
            SubagentState::Done
        );
        assert_eq!(
            ChildFacts {
                outcome: Some(TurnOutcome::Settled),
                ..silent
            }
            .state(),
            SubagentState::Done
        );
        assert_eq!(
            ChildFacts {
                live: Some(LiveStatus::Idle),
                ..silent
            }
            .state(),
            SubagentState::Done
        );
    }

    #[test]
    fn the_host_snapshot_stops_at_a_space() {
        let host = SessionRecord::new("ses_host", "/repo");
        let worktree = SessionRecord::new("ses_worktree", "/repo/.worktrees/a");
        let space = SessionRecord::new("ses_space", "/spaces/0123456789ab/repo");
        let nowhere = SessionRecord {
            directory: None,
            ..SessionRecord::new("ses_nowhere", "/repo")
        };
        assert!(host_snapshot_boundary(&host, &worktree));
        assert!(!host_snapshot_boundary(&host, &space));
        assert!(!host_snapshot_boundary(&space, &host));
        assert!(
            !host_snapshot_boundary(&space, &space),
            "a space's own snapshot is not the host's"
        );
        assert!(!host_snapshot_boundary(&host, &nowhere));
    }

    #[test]
    fn membership_is_direct_children_without_workflow_agents() {
        let mut marked = child("ses_marked", 5);
        marked.kind = SessionKind::WorkflowAgent;
        let sessions = vec![
            SessionRecord::new(PARENT, "/repo"),
            child("ses_a", 1),
            marked,
            child("ses_run_agent", 3),
            with_parent(child("ses_grandchild", 4), "ses_a"),
            with_parent(child("ses_other", 6), "ses_elsewhere"),
        ];
        let agents: HashSet<SessionId> = [SessionId::from("ses_run_agent")].into();
        let rows = rows(&sessions, &SessionId::from(PARENT), &agents, |_| BUSY);
        assert_eq!(ids(&rows), ["ses_a"]);
        assert!(
            listed(&sessions[..1], |_| BUSY).is_empty(),
            "no children, no rows"
        );
    }

    #[test]
    fn groups_order_by_rank_then_newest_creation() {
        let sessions = vec![
            child("ses_done_old", 1),
            child("ses_unknown", 2),
            child("ses_working_old", 3),
            child("ses_asked", 4),
            child("ses_failed_new", 5),
            child("ses_working_new", 6),
            child("ses_blocked", 7),
        ];
        let facts = |session: &SessionRecord| match session.id.as_str() {
            "ses_blocked" => ChildFacts {
                permission: true,
                ..ChildFacts::default()
            },
            "ses_asked" => ChildFacts {
                question: true,
                ..BUSY
            },
            "ses_working_old" | "ses_working_new" => BUSY,
            "ses_failed_new" => ChildFacts {
                outcome: Some(TurnOutcome::Failed),
                ..ChildFacts::default()
            },
            "ses_done_old" => ChildFacts {
                outcome: Some(TurnOutcome::Settled),
                ..ChildFacts::default()
            },
            _ => ChildFacts::default(),
        };
        let rows = listed(&sessions, facts);
        assert_eq!(
            ids(&rows),
            [
                "ses_blocked",
                "ses_asked",
                "ses_working_new",
                "ses_working_old",
                "ses_unknown",
                "ses_failed_new",
                "ses_done_old",
            ]
        );
        assert_eq!(summary(&rows), "3/7", "the asking child still runs");
    }

    #[test]
    fn activity_does_not_reorder_a_group() {
        let mut sessions = vec![child("ses_old", 1), child("ses_new", 2)];
        let before = ids(&listed(&sessions, |_| BUSY))
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        // The older child reports activity: only its update time moves.
        sessions[0].time.updated = 99;
        let after = listed(&sessions, |_| BUSY);
        assert_eq!(ids(&after), before);
        assert_eq!(ids(&after), ["ses_new", "ses_old"]);

        let settled = listed(&sessions, |_| ChildFacts {
            snapshot_covers: true,
            ..ChildFacts::default()
        });
        assert_eq!(ids(&settled), ["ses_new", "ses_old"]);
        assert_eq!(summary(&settled), "2");
    }

    #[test]
    fn cost_rolls_up_each_subtree_and_survives_a_cycle() {
        let sessions = vec![
            with_cost(SessionRecord::new(PARENT, "/repo"), 100.0),
            with_cost(child("ses_a", 1), 0.25),
            with_cost(with_parent(child("ses_a1", 2), "ses_a"), 0.5),
            with_cost(with_parent(child("ses_a2", 3), "ses_a1"), 1.0),
            child("ses_free", 4),
            // A cycle: `ses_loop` and `ses_loop_child` name each other, and
            // `ses_loop` is also listed under the parent.
            with_cost(child("ses_loop", 5), 2.0),
            with_cost(with_parent(child("ses_loop_child", 6), "ses_loop"), 3.0),
            with_cost(with_parent(child("ses_loop", 7), "ses_loop_child"), 2.0),
        ];
        let rows = listed(&sessions, |_| BUSY);
        let cost = |id: &str| {
            rows.iter()
                .find(|row| row.session.id.as_str() == id)
                .expect("the child is listed")
                .cost
        };
        assert_eq!(cost("ses_a"), Usd::from_dollars(1.75));
        assert_eq!(cost("ses_free"), None, "zero cost shows nothing");
        assert_eq!(
            cost("ses_loop"),
            Usd::from_dollars(5.0),
            "each session of a cycle counts once"
        );
    }

    #[test]
    fn one_child_without_facts_keeps_the_others() {
        let mut broken = child("ses_broken", 2);
        broken.directory = None;
        broken.title = String::new();
        let sessions = vec![with_cost(child("ses_ok", 1), 0.5), broken];
        let rows = listed(&sessions, |session| {
            if session.directory.is_none() {
                ChildFacts::default()
            } else {
                BUSY
            }
        });
        assert_eq!(ids(&rows), ["ses_ok", "ses_broken"]);
        assert_eq!(rows[0].state, SubagentState::Working);
        assert_eq!(rows[0].cost, Usd::from_dollars(0.5));
        assert_eq!(rows[1].state, SubagentState::Unknown);
    }

    /// Expected strings are `x.toFixed(4)` from Node for the same literals.
    #[test]
    fn four_decimals_round_as_to_fixed_does() {
        // Exact binary ties: the larger neighbour, where `{:.4}` picks the
        // even one for the first, second and fourth.
        assert_eq!(format!("{:.4}", 0.03125_f64), "0.0312");
        assert_eq!(to_fixed_4(0.03125), "0.0313");
        assert_eq!(to_fixed_4(0.15625), "0.1563");
        assert_eq!(to_fixed_4(0.09375), "0.0938");
        assert_eq!(to_fixed_4(0.28125), "0.2813");
        assert_eq!(to_fixed_4(3.21875), "3.2188");
        // Decimal midpoints whose float is not the midpoint: the side the
        // float lies on.
        assert_eq!(to_fixed_4(12.34565), "12.3456");
        assert_eq!(to_fixed_4(0.00015), "0.0001");
        assert_eq!(to_fixed_4(1.00005), "1.0001");
        assert_eq!(to_fixed_4(0.00005), "0.0001");
        assert_eq!(to_fixed_4(0.99995), "1.0000");
        // Everything else.
        assert_eq!(to_fixed_4(0.0), "0.0000");
        assert_eq!(to_fixed_4(7.0), "7.0000");
        assert_eq!(to_fixed_4(2.5), "2.5000");
        assert_eq!(to_fixed_4(0.0123), "0.0123");
        assert_eq!(to_fixed_4(0.000049999), "0.0000");
    }

    #[test]
    fn a_cost_on_an_exact_tie_rounds_up() {
        let format = |dollars: f64| format_cost(Usd::from_dollars(dollars).expect("a valid cost"));
        assert_eq!(format(0.03125), "$0.0313");
        assert_eq!(format(0.15625), "$0.1563");
        assert_eq!(format(3.21875), "$3.2188");
        assert_eq!(format(12.34565), "$12.3456");
        assert_eq!(format(0.00015), "$0.0001");
        // Two children at 0.015625 each: the exact sum is the tie 0.03125.
        let half = Usd::from_dollars(0.015625).expect("a valid cost");
        assert_eq!(format_cost(half + half), "$0.0313");
    }

    #[test]
    fn cost_keeps_four_decimals_without_trailing_zeros() {
        let format = |dollars: f64| format_cost(Usd::from_dollars(dollars).expect("a valid cost"));
        assert_eq!(format(0.0123), "$0.0123");
        assert_eq!(format(1.5), "$1.5");
        assert_eq!(format(2.0), "$2");
        assert_eq!(format(0.00004), "$0");
        assert_eq!(format(12.34567), "$12.3457");
    }

    #[test]
    fn durations_use_the_sidebar_readout_steps() {
        assert_eq!(
            ActivityDuration::since(1_000, 8_999),
            ActivityDuration::Seconds(7)
        );
        assert_eq!(
            ActivityDuration::since(9_000, 1_000),
            ActivityDuration::Seconds(0)
        );
        assert_eq!(
            ActivityDuration::since(0, 83_500),
            ActivityDuration::MinutesSeconds {
                minutes: 1,
                seconds: 23
            }
        );
        assert_eq!(
            ActivityDuration::since(0, 3_725_000),
            ActivityDuration::HoursMinutes {
                hours: 1,
                minutes: 2
            }
        );
    }
}
