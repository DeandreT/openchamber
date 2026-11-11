//! Workflow runs as the OpenChamber server summarizes them and the store that
//! keeps one session's runs current.
//!
//! Mirrors `packages/ui/src/lib/workflows/model.ts` and
//! `packages/ui/src/lib/workflows/store.ts`. A session's runs are read once;
//! after that the server's `openchamber:workflow-run` broadcasts replace each
//! run. Those broadcasts carry no revision, so the store orders them against
//! reads itself: a run an event touched after a read started is newer than
//! what that read returns. A failed read keeps the runs already held.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::ids::SessionId;

/// A workflow run's server identifier. Run identifiers are never reused.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkflowRunId(String);

impl WorkflowRunId {
    /// Wraps an identifier received from the server.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The identifier as received.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for WorkflowRunId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Where a run stands (`runStateSchema`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowRunState {
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Stopped,
    Declined,
}

impl WorkflowRunState {
    /// The state named by the server's spelling.
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "awaiting-approval" => Self::AwaitingApproval,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            "declined" => Self::Declined,
            _ => return None,
        })
    }
}

/// Where one agent of a run stands (`agentStateSchema`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowAgentState {
    Queued,
    Running,
    Completed,
    Failed,
    Stopped,
}

impl WorkflowAgentState {
    /// The state named by the server's spelling.
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => return None,
        })
    }
}

/// What the user can ask of a run (`WorkflowRunAction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowRunAction {
    Approve,
    Decline,
    Stop,
    Resume,
}

impl WorkflowRunAction {
    /// The action's route segment.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Decline => "decline",
            Self::Stop => "stop",
            Self::Resume => "resume",
        }
    }
}

/// One agent of a run, as the work status card reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowAgent {
    pub key: String,
    pub label: String,
    /// The phase it started in, absent outside any phase.
    pub phase: Option<String>,
    pub state: WorkflowAgentState,
    /// The agent's own session, once the server created it.
    pub session_id: Option<SessionId>,
    /// Its answer was reused from an earlier attempt.
    pub replayed: bool,
    pub error: Option<String>,
    pub preview: Option<String>,
}

/// A workflow run's summary (`summarizeRun`), without the fields only the
/// Workflows panel reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    /// The session that proposed the run.
    pub parent_session_id: SessionId,
    pub directory: String,
    pub name: String,
    pub description: String,
    pub state: WorkflowRunState,
    /// Milliseconds since the Unix epoch; runs list oldest first.
    pub created_at: i64,
    pub current_phase: Option<String>,
    /// The planned phases, in plan order.
    pub phases: Vec<String>,
    pub agents: Vec<WorkflowAgent>,
    pub error: Option<String>,
}

/// One phase of a run with its agents (`groupWorkflowAgentsByPhase`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseGroup<'run> {
    /// Absent for agents started outside a phase.
    pub title: Option<&'run str>,
    pub agents: Vec<&'run WorkflowAgent>,
    /// How many of the agents completed.
    pub done: usize,
}

impl WorkflowRun {
    /// Whether the run is still going or still waiting for the user
    /// (`isWorkflowRunOpen`).
    pub fn is_open(&self) -> bool {
        matches!(
            self.state,
            WorkflowRunState::Running | WorkflowRunState::AwaitingApproval
        )
    }

    /// The agents grouped by phase, in plan order: the planned phases first,
    /// then any phase the script named on the way, then agents started
    /// outside a phase where they first appear. A planned phase nobody
    /// reached yet has no agents.
    pub fn phase_groups(&self) -> Vec<PhaseGroup<'_>> {
        let mut groups: Vec<(Option<&str>, Vec<&WorkflowAgent>)> = Vec::new();
        for title in &self.phases {
            if !groups.iter().any(|(held, _)| *held == Some(title.as_str())) {
                groups.push((Some(title.as_str()), Vec::new()));
            }
        }
        for agent in &self.agents {
            let phase = agent.phase.as_deref();
            match groups.iter_mut().find(|(held, _)| *held == phase) {
                Some((_, agents)) => agents.push(agent),
                None => groups.push((phase, vec![agent])),
            }
        }
        groups
            .into_iter()
            .filter(|(title, agents)| title.is_some() || !agents.is_empty())
            .map(|(title, agents)| PhaseGroup {
                title,
                done: agents
                    .iter()
                    .filter(|agent| agent.state == WorkflowAgentState::Completed)
                    .count(),
                agents,
            })
            .collect()
    }

    /// Where a running run is, for its one-line row: the current phase and
    /// how many of its agents finished (`runProgress`). Absent for a run that
    /// is not running or whose current phase has no agents yet.
    pub fn progress(&self) -> Option<String> {
        if self.state != WorkflowRunState::Running {
            return None;
        }
        let current = self.current_phase.as_deref();
        let groups = self.phase_groups();
        let phase = groups
            .iter()
            .find(|group| group.title == current && !group.agents.is_empty())?;
        let text = format!(
            "{} {}/{}",
            phase.title.unwrap_or_default(),
            phase.done,
            phase.agents.len()
        );
        Some(text.trim().to_owned())
    }

    /// The sessions of this run's agents.
    pub fn agent_session_ids(&self) -> impl Iterator<Item = &SessionId> {
        self.agents
            .iter()
            .filter_map(|agent| agent.session_id.as_ref())
    }
}

/// Where a session's read of its runs stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunsStatus {
    /// Nothing asked for the session yet, or its runs must be read again.
    #[default]
    Idle,
    Loading,
    Ready,
    /// The newest read failed; the runs held before it stay.
    Failed,
}

#[derive(Debug, Default)]
struct SessionRuns {
    status: RunsStatus,
    /// Oldest first.
    runs: Vec<WorkflowRun>,
}

/// The read a caller started with [`WorkflowRunStore::begin_load`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a started load must be finished or its session stays loading"]
pub struct LoadTicket {
    session: SessionId,
    request: u64,
    started_at: u64,
}

impl LoadTicket {
    /// The session whose runs the read asks for.
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

/// The workflow runs of every session a surface asked about, in one runtime.
#[derive(Debug, Default)]
pub struct WorkflowRunStore {
    by_session: HashMap<SessionId, SessionRuns>,
    /// Counts applied events; a run's entry in `touched_at` is the count at
    /// its newest event.
    event_sequence: u64,
    touched_at: HashMap<WorkflowRunId, u64>,
    /// Never restarts, so a read that was on its way when the runtime
    /// changed cannot pass for a newer one.
    request_sequence: u64,
    /// Each session's newest read.
    load_sequence: HashMap<SessionId, u64>,
    /// Deleted runs stay out of every later read, including one that was on
    /// its way when the run was deleted.
    removed: HashSet<WorkflowRunId>,
}

fn upsert(runs: &mut Vec<WorkflowRun>, run: WorkflowRun) {
    match runs.iter_mut().find(|held| held.id == run.id) {
        Some(held) => *held = run,
        None => {
            runs.push(run);
            // Stable, so runs created in the same millisecond keep the
            // order they arrived in.
            runs.sort_by_key(|run| run.created_at);
        }
    }
}

impl WorkflowRunStore {
    /// The session's runs, oldest first; empty before any read or event.
    pub fn runs(&self, session: &SessionId) -> &[WorkflowRun] {
        self.by_session
            .get(session)
            .map(|entry| entry.runs.as_slice())
            .unwrap_or_default()
    }

    /// Where the session's read stands.
    pub fn status(&self, session: &SessionId) -> RunsStatus {
        self.by_session
            .get(session)
            .map_or(RunsStatus::Idle, |entry| entry.status)
    }

    /// Whether a surface showing the session should start a read: nothing
    /// was read yet, or the last read failed. While a read runs or after one
    /// landed, events and reconnects keep the list current.
    pub fn needs_load(&self, session: &SessionId) -> bool {
        matches!(self.status(session), RunsStatus::Idle | RunsStatus::Failed)
    }

    /// Marks the session loading and names the read to finish with
    /// [`Self::finish_load`]. An older read of the session is superseded.
    pub fn begin_load(&mut self, session: &SessionId) -> LoadTicket {
        self.request_sequence += 1;
        self.load_sequence
            .insert(session.clone(), self.request_sequence);
        self.by_session.entry(session.clone()).or_default().status = RunsStatus::Loading;
        LoadTicket {
            session: session.clone(),
            request: self.request_sequence,
            started_at: self.event_sequence,
        }
    }

    /// Takes a read's answer, `None` when it failed. Returns whether the
    /// store changed: a superseded read, or one started before
    /// [`Self::reset`], changes nothing.
    ///
    /// A successful read replaces the session's runs, except that a run an
    /// event touched after the read started keeps its newer state and a
    /// deleted run stays out. A failed read keeps every run held.
    pub fn finish_load(&mut self, ticket: &LoadTicket, fetched: Option<Vec<WorkflowRun>>) -> bool {
        if self.load_sequence.get(&ticket.session) != Some(&ticket.request) {
            return false;
        }
        let entry = self.by_session.entry(ticket.session.clone()).or_default();
        let Some(fetched) = fetched else {
            entry.status = RunsStatus::Failed;
            return true;
        };
        let mut runs: Vec<WorkflowRun> = fetched
            .into_iter()
            .filter(|run| !self.removed.contains(&run.id))
            .collect();
        for held in std::mem::take(&mut entry.runs) {
            let newer = self
                .touched_at
                .get(&held.id)
                .is_some_and(|touched| *touched > ticket.started_at);
            if newer {
                upsert(&mut runs, held);
            }
        }
        entry.runs = runs;
        entry.status = RunsStatus::Ready;
        true
    }

    /// Applies a run the server announced. Returns whether the store
    /// changed: a deleted run stays deleted.
    pub fn apply(&mut self, run: WorkflowRun) -> bool {
        if self.removed.contains(&run.id) {
            return false;
        }
        self.event_sequence += 1;
        self.touched_at.insert(run.id.clone(), self.event_sequence);
        let entry = self
            .by_session
            .entry(run.parent_session_id.clone())
            .or_default();
        upsert(&mut entry.runs, run);
        true
    }

    /// Drops a deleted run. Returns whether a held run went away. The
    /// deletion is remembered even when the run is not held, so a read on
    /// its way cannot bring it back.
    pub fn remove(&mut self, session: &SessionId, run: &WorkflowRunId) -> bool {
        self.removed.insert(run.clone());
        self.touched_at.remove(run);
        let Some(entry) = self.by_session.get_mut(session) else {
            return false;
        };
        let before = entry.runs.len();
        entry.runs.retain(|held| held.id != *run);
        entry.runs.len() != before
    }

    /// The sessions a surface has asked about, which a reconnect reads again
    /// because changes during the gap were not replayed (`reloadLoaded`).
    pub fn loaded_sessions(&self) -> Vec<SessionId> {
        self.by_session
            .iter()
            .filter(|(_, entry)| entry.status != RunsStatus::Idle)
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// Marks every session unread while workflows are not shown, so its
    /// surfaces read it again once workflows are turned on. In-flight reads
    /// are superseded; held runs stay.
    pub fn mark_unread(&mut self) {
        self.load_sequence.clear();
        for entry in self.by_session.values_mut() {
            entry.status = RunsStatus::Idle;
        }
    }

    /// Forgets everything, as a new runtime must. Reads started before the
    /// reset no longer land.
    pub fn reset(&mut self) {
        self.by_session.clear();
        self.touched_at.clear();
        self.load_sequence.clear();
        self.removed.clear();
    }

    /// The sessions of the agents of every run `parent` proposed. The
    /// Subagents section leaves these out, so each session shows in one
    /// place.
    pub fn agent_session_ids(&self, parent: &SessionId) -> HashSet<SessionId> {
        self.runs(parent)
            .iter()
            .flat_map(WorkflowRun::agent_session_ids)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(key: &str, phase: Option<&str>, state: WorkflowAgentState) -> WorkflowAgent {
        WorkflowAgent {
            key: key.to_owned(),
            label: key.to_owned(),
            phase: phase.map(str::to_owned),
            state,
            session_id: Some(SessionId::from(format!("ses_{key}"))),
            replayed: false,
            error: None,
            preview: None,
        }
    }

    fn run(id: &str, created_at: i64, state: WorkflowRunState) -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId::from(id),
            parent_session_id: SessionId::from("ses_parent"),
            directory: "/repo".to_owned(),
            name: id.to_owned(),
            description: String::new(),
            state,
            created_at,
            current_phase: None,
            phases: Vec::new(),
            agents: Vec::new(),
            error: None,
        }
    }

    fn ids(store: &WorkflowRunStore, session: &SessionId) -> Vec<String> {
        store
            .runs(session)
            .iter()
            .map(|run| run.id.to_string())
            .collect()
    }

    #[test]
    fn phases_group_in_plan_order_then_encounter_order() {
        let mut subject = run("r", 1, WorkflowRunState::Running);
        subject.phases = vec!["plan".to_owned(), "build".to_owned(), "plan".to_owned()];
        subject.agents = vec![
            agent("a", Some("build"), WorkflowAgentState::Completed),
            agent("b", None, WorkflowAgentState::Running),
            agent("c", Some("extra"), WorkflowAgentState::Failed),
            agent("d", Some("build"), WorkflowAgentState::Running),
        ];
        let groups = subject.phase_groups();
        let titles: Vec<Option<&str>> = groups.iter().map(|group| group.title).collect();
        assert_eq!(
            titles,
            [Some("plan"), Some("build"), None, Some("extra")],
            "planned phases come first, a repeated plan entry is one group"
        );
        assert!(
            groups[0].agents.is_empty(),
            "an unreached phase stays listed"
        );
        assert_eq!((groups[1].done, groups[1].agents.len()), (1, 2));
    }

    #[test]
    fn progress_names_the_current_phase_of_a_running_run_only() {
        let mut subject = run("r", 1, WorkflowRunState::Running);
        subject.phases = vec!["build".to_owned()];
        subject.current_phase = Some("build".to_owned());
        assert_eq!(
            subject.progress(),
            None,
            "a phase without agents says nothing"
        );
        subject.agents = vec![
            agent("a", Some("build"), WorkflowAgentState::Completed),
            agent("b", Some("build"), WorkflowAgentState::Running),
        ];
        assert_eq!(subject.progress().as_deref(), Some("build 1/2"));
        subject.state = WorkflowRunState::Completed;
        assert_eq!(subject.progress(), None);

        let mut unphased = run("u", 1, WorkflowRunState::Running);
        unphased.agents = vec![agent("a", None, WorkflowAgentState::Completed)];
        assert_eq!(
            unphased.progress().as_deref(),
            Some("1/1"),
            "agents outside a phase count without a title"
        );
    }

    #[test]
    fn a_read_lists_runs_and_an_empty_answer_is_ready_and_empty() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        assert!(store.needs_load(&session));
        let ticket = store.begin_load(&session);
        assert_eq!(store.status(&session), RunsStatus::Loading);
        assert!(!store.needs_load(&session), "one read at a time");
        assert!(store.finish_load(
            &ticket,
            Some(vec![
                run("new", 20, WorkflowRunState::Running),
                run("old", 10, WorkflowRunState::Completed),
            ])
        ));
        assert_eq!(store.status(&session), RunsStatus::Ready);
        assert_eq!(
            ids(&store, &session),
            ["new", "old"],
            "the server's order is kept"
        );

        let other = SessionId::from("ses_other");
        let ticket = store.begin_load(&other);
        assert!(store.finish_load(&ticket, Some(Vec::new())));
        assert_eq!(store.status(&other), RunsStatus::Ready);
        assert!(store.runs(&other).is_empty());
        assert!(!store.needs_load(&other), "an empty answer is an answer");
    }

    #[test]
    fn a_failed_first_read_is_not_an_empty_answer_and_is_retried() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        let ticket = store.begin_load(&session);
        assert!(store.finish_load(&ticket, None));
        assert_eq!(store.status(&session), RunsStatus::Failed);
        assert!(store.needs_load(&session), "the next surface reads again");
    }

    #[test]
    fn a_failed_reread_keeps_the_runs_held() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        let ticket = store.begin_load(&session);
        store.finish_load(&ticket, Some(vec![run("a", 1, WorkflowRunState::Running)]));
        let ticket = store.begin_load(&session);
        assert_eq!(
            ids(&store, &session),
            ["a"],
            "a reread keeps the rows on screen"
        );
        assert!(store.finish_load(&ticket, None));
        assert_eq!(store.status(&session), RunsStatus::Failed);
        assert_eq!(ids(&store, &session), ["a"]);
    }

    #[test]
    fn an_event_during_a_read_outlives_the_reads_older_answer() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        store.apply(run("a", 1, WorkflowRunState::AwaitingApproval));
        let ticket = store.begin_load(&session);
        assert!(store.apply(run("a", 1, WorkflowRunState::Running)));
        assert!(store.apply(run("b", 2, WorkflowRunState::AwaitingApproval)));
        store.finish_load(
            &ticket,
            Some(vec![run("a", 1, WorkflowRunState::AwaitingApproval)]),
        );
        assert_eq!(ids(&store, &session), ["a", "b"]);
        assert_eq!(store.runs(&session)[0].state, WorkflowRunState::Running);

        // An event from before the read started does not outrank it.
        let ticket = store.begin_load(&session);
        store.finish_load(
            &ticket,
            Some(vec![run("a", 1, WorkflowRunState::Completed)]),
        );
        assert_eq!(ids(&store, &session), ["a"]);
        assert_eq!(store.runs(&session)[0].state, WorkflowRunState::Completed);
    }

    #[test]
    fn a_superseded_read_and_a_read_from_before_a_reset_change_nothing() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        let first = store.begin_load(&session);
        let second = store.begin_load(&session);
        assert!(!store.finish_load(
            &first,
            Some(vec![run("stale", 1, WorkflowRunState::Running)])
        ));
        assert!(store.runs(&session).is_empty());
        assert!(store.finish_load(&second, Some(vec![run("a", 1, WorkflowRunState::Running)])));

        let retired = store.begin_load(&session);
        store.reset();
        assert!(store.runs(&session).is_empty());
        assert!(
            !store.finish_load(
                &retired,
                Some(vec![run("other", 1, WorkflowRunState::Running)])
            ),
            "another runtime's answer never lands"
        );
        assert!(store.runs(&session).is_empty());
        assert_eq!(store.status(&session), RunsStatus::Idle);
        let fresh = store.begin_load(&session);
        assert_ne!(fresh, retired, "request numbers never restart");
    }

    #[test]
    fn a_deleted_run_stays_out_of_events_and_reads() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        store.apply(run("a", 1, WorkflowRunState::Completed));
        store.apply(run("b", 2, WorkflowRunState::Completed));
        let ticket = store.begin_load(&session);
        assert!(store.remove(&session, &WorkflowRunId::from("a")));
        assert!(!store.apply(run("a", 1, WorkflowRunState::Completed)));
        store.finish_load(
            &ticket,
            Some(vec![
                run("a", 1, WorkflowRunState::Completed),
                run("b", 2, WorkflowRunState::Completed),
            ]),
        );
        assert_eq!(ids(&store, &session), ["b"], "the other run is untouched");
        assert!(
            !store.remove(&SessionId::from("ses_unknown"), &WorkflowRunId::from("c")),
            "a deletion for a session nobody asked about holds nothing"
        );
        assert!(!store.apply(run("c", 3, WorkflowRunState::Running)));
    }

    #[test]
    fn one_sessions_failure_leaves_another_sessions_runs() {
        let (first, second) = (SessionId::from("ses_parent"), SessionId::from("ses_two"));
        let mut store = WorkflowRunStore::default();
        let ticket = store.begin_load(&first);
        store.finish_load(&ticket, Some(vec![run("a", 1, WorkflowRunState::Running)]));
        let ticket = store.begin_load(&second);
        store.finish_load(&ticket, None);
        assert_eq!(ids(&store, &first), ["a"]);
        assert_eq!(store.status(&first), RunsStatus::Ready);
        assert_eq!(store.status(&second), RunsStatus::Failed);
    }

    #[test]
    fn reconnects_reread_asked_sessions_and_turning_workflows_off_marks_them_unread() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        store.apply(run("a", 1, WorkflowRunState::Running));
        assert!(
            store.loaded_sessions().is_empty(),
            "a session only events named was never asked about"
        );
        let ticket = store.begin_load(&session);
        store.finish_load(&ticket, Some(vec![run("a", 1, WorkflowRunState::Running)]));
        assert_eq!(store.loaded_sessions(), std::slice::from_ref(&session));

        let in_flight = store.begin_load(&session);
        store.mark_unread();
        assert_eq!(store.status(&session), RunsStatus::Idle);
        assert!(!store.finish_load(&in_flight, Some(Vec::new())));
        assert_eq!(
            ids(&store, &session),
            ["a"],
            "held runs stay for the next read"
        );
        assert!(store.needs_load(&session));
    }

    #[test]
    fn agent_sessions_of_a_parents_runs_are_its_membership() {
        let session = SessionId::from("ses_parent");
        let mut store = WorkflowRunStore::default();
        let mut first = run("a", 1, WorkflowRunState::Running);
        first.agents = vec![
            agent("one", None, WorkflowAgentState::Running),
            WorkflowAgent {
                session_id: None,
                ..agent("queued", None, WorkflowAgentState::Queued)
            },
        ];
        let mut second = run("b", 2, WorkflowRunState::Completed);
        second.agents = vec![agent("two", None, WorkflowAgentState::Completed)];
        store.apply(first);
        store.apply(second);
        let members = store.agent_session_ids(&session);
        assert_eq!(
            members,
            HashSet::from([SessionId::from("ses_one"), SessionId::from("ses_two")])
        );
        assert!(
            store
                .agent_session_ids(&SessionId::from("ses_other"))
                .is_empty()
        );
    }
}
