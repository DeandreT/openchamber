//! Scheduled tasks (`/api/projects/:projectId/scheduled-tasks`), as
//! `packages/ui/src/lib/scheduledTasksApi.ts` calls them: the list, a save,
//! a delete, run now, a repository loop's enable and delete, and which
//! sessions a task may target.
//!
//! The project id is the settings id of a registered project, or
//! `openchamber:chats` for the Chats scope. Each answer carries the whole
//! list as stored. A failed or malformed read is an error, never an empty
//! list; the server's own explanation travels with a refusal.

use std::collections::HashSet;

use openchamber_native_core::schedule::{
    RunStatus, Schedule, ScheduleKind, SessionMode, TaskDraft,
};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Map, Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;

/// The scope of chats; each of its runs starts a new chat.
pub const CHATS_SCOPE: &str = "openchamber:chats";
/// The eligibility route takes at most this many candidates.
const ELIGIBLE_BATCH: usize = 500;

/// Why a repository loop that its file enables is paused here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoopApproval {
    /// Never enabled on this machine.
    Required,
    /// Changed since it was.
    Outdated,
}

/// What a task runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Execution {
    pub prompt: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub use_defaults: bool,
    pub variant: Option<String>,
    pub agent: Option<String>,
    pub goal_enabled: bool,
    pub goal_token_budget: Option<i64>,
    pub permission_auto_accept: bool,
}

/// What the scheduler knows about a task's runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskState {
    pub last_run_at: Option<i64>,
    pub last_status: RunStatus,
    pub last_error: Option<String>,
    pub last_session_id: Option<String>,
    pub last_run_isolated: bool,
    pub last_run_branch: Option<String>,
    pub next_run_at: Option<i64>,
}

/// A stored task.
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub target_session_id: Option<String>,
    pub loop_file: Option<String>,
    pub loop_approval: Option<LoopApproval>,
    pub schedule: Schedule,
    pub execution: Execution,
    pub run_in_worktree: bool,
    pub cleanup_worktree: bool,
    pub base_branch: Option<String>,
    pub state: TaskState,
    /// The task as the server sent it, so a toggle writes back every field,
    /// including ones this client does not know.
    pub raw: Map<String, Value>,
}

/// A run now that the server accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunStarted {
    pub session_id: Option<String>,
    /// Where the run's session lives.
    pub directory: Option<String>,
    /// The run started, but its state could not be saved.
    pub persist_error: Option<String>,
}

/// Why a scheduled task request did not succeed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduledTaskError {
    /// The task already has a run in flight (`running`) or waiting (`queued`).
    #[error("the scheduled task is already {0}")]
    Busy(&'static str),
    /// The server declined, with its explanation when it sent one.
    #[error("the server refused the scheduled task request (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl ScheduledTaskError {
    /// The server's explanation, when it gave one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            _ => None,
        }
    }
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn flag(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool) == Some(true)
}

fn number(object: &Map<String, Value>, key: &str) -> Option<i64> {
    object
        .get(key)?
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value as i64)
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
    )
}

/// A task with an id, a name and a schedule; anything else is dropped from
/// the list on its own.
fn decode_task(value: &Value) -> Option<ScheduledTask> {
    let object = value.as_object()?;
    let id = text(object, "id")?;
    let name = object.get("name")?.as_str()?.to_owned();
    let schedule = object.get("schedule")?.as_object()?;
    let empty = Map::new();
    let execution = object
        .get("execution")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let state = object
        .get("state")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    Some(ScheduledTask {
        id,
        name,
        enabled: flag(object, "enabled"),
        target_session_id: text(object, "targetSessionId"),
        loop_file: text(object, "loopFile"),
        loop_approval: match object.get("loopApproval").and_then(Value::as_str) {
            Some("required") => Some(LoopApproval::Required),
            Some("outdated") => Some(LoopApproval::Outdated),
            _ => None,
        },
        schedule: Schedule {
            kind: ScheduleKind::parse(schedule.get("kind").and_then(Value::as_str)?),
            times: strings(schedule.get("times")),
            time: text(schedule, "time"),
            date: text(schedule, "date"),
            weekdays: schedule.get("weekdays").and_then(Value::as_array).map(|days| {
                days.iter()
                    .filter_map(|day| day.as_f64().map(|day| day as i64))
                    .collect()
            }),
            cron: text(schedule, "cron"),
            timezone: text(schedule, "timezone"),
        },
        execution: Execution {
            prompt: execution
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            provider_id: text(execution, "providerID"),
            model_id: text(execution, "modelID"),
            use_defaults: flag(execution, "useDefaults"),
            variant: text(execution, "variant"),
            agent: text(execution, "agent"),
            goal_enabled: flag(execution, "goalEnabled"),
            goal_token_budget: number(execution, "goalTokenBudget"),
            permission_auto_accept: flag(execution, "permissionAutoAccept"),
        },
        run_in_worktree: flag(object, "runInWorktree"),
        cleanup_worktree: object.get("cleanupWorktree").and_then(Value::as_bool) != Some(false),
        base_branch: text(object, "baseBranch"),
        state: TaskState {
            last_run_at: number(state, "lastRunAt"),
            last_status: state
                .get("lastStatus")
                .and_then(Value::as_str)
                .map(RunStatus::parse)
                .unwrap_or_default(),
            last_error: text(state, "lastError"),
            last_session_id: text(state, "lastSessionId"),
            last_run_isolated: flag(state, "lastRunIsolated"),
            last_run_branch: text(state, "lastRunBranch"),
            next_run_at: number(state, "nextRunAt"),
        },
        raw: object.clone(),
    })
}

fn decode_tasks(body: &Value) -> Option<Vec<ScheduledTask>> {
    Some(
        body.get("tasks")?
            .as_array()?
            .iter()
            .filter_map(decode_task)
            .collect(),
    )
}

/// `buildScheduledTaskPayload`: what a save sends for a draft. A new-session
/// task also sends its isolation choices and the stored run state.
pub fn task_payload(draft: &TaskDraft, state: Option<&Value>) -> Value {
    let mut schedule = Map::new();
    schedule.insert("kind".into(), json!(draft.kind.as_str()));
    schedule.insert("timezone".into(), json!(draft.timezone.trim()));
    match draft.kind {
        ScheduleKind::Cron => {
            schedule.insert("cron".into(), json!(draft.cron.trim()));
        }
        ScheduleKind::Once => {
            schedule.insert("date".into(), json!(draft.once_date));
            schedule.insert("time".into(), json!(draft.once_time));
        }
        ScheduleKind::Daily | ScheduleKind::Weekly => {
            schedule.insert("times".into(), json!(draft.saved_times()));
            if draft.kind == ScheduleKind::Weekly {
                schedule.insert("weekdays".into(), json!(draft.weekdays));
            }
        }
    }
    let mut execution = Map::new();
    execution.insert("prompt".into(), json!(draft.prompt));
    if draft.use_defaults {
        execution.insert("useDefaults".into(), json!(true));
    }
    execution.insert("providerID".into(), json!(draft.provider_id));
    execution.insert("modelID".into(), json!(draft.model_id));
    if !draft.variant.trim().is_empty() {
        execution.insert("variant".into(), json!(draft.variant.trim()));
    }
    if !draft.agent.trim().is_empty() {
        execution.insert("agent".into(), json!(draft.agent.trim()));
    }
    if draft.permission_auto_accept {
        execution.insert("permissionAutoAccept".into(), json!(true));
    }
    if draft.goal_enabled {
        execution.insert("goalEnabled".into(), json!(true));
        if let Some(budget) = draft.goal_token_budget {
            execution.insert("goalTokenBudget".into(), json!(budget));
        }
    }
    let mut payload = Map::new();
    if let Some(id) = &draft.id {
        payload.insert("id".into(), json!(id));
    }
    payload.insert("name".into(), json!(draft.name.trim()));
    let existing = draft.session_mode == SessionMode::Existing;
    payload.insert(
        "targetSessionId".into(),
        json!(if existing {
            draft.target_session_id.as_str()
        } else {
            ""
        }),
    );
    payload.insert("enabled".into(), json!(draft.enabled));
    payload.insert("schedule".into(), Value::Object(schedule));
    payload.insert("execution".into(), Value::Object(execution));
    if !existing {
        payload.insert("runInWorktree".into(), json!(draft.run_in_worktree));
        payload.insert("baseBranch".into(), json!(draft.base_branch.trim()));
        payload.insert("cleanupWorktree".into(), json!(draft.cleanup_worktree));
        if let Some(state) = state {
            payload.insert("state".into(), state.clone());
        }
    }
    Value::Object(payload)
}

/// `toDraft(task, …)`: the editor's form for a stored task.
pub fn task_draft(task: &ScheduledTask, defaults: &openchamber_native_core::schedule::DraftDefaults) -> TaskDraft {
    let times = openchamber_native_core::schedule::schedule_times(&task.schedule);
    TaskDraft {
        id: Some(task.id.clone()),
        session_mode: if task.target_session_id.is_some() {
            SessionMode::Existing
        } else {
            SessionMode::New
        },
        target_session_id: task.target_session_id.clone().unwrap_or_default(),
        name: task.name.clone(),
        enabled: task.enabled,
        kind: task.schedule.kind,
        times: if times.is_empty() {
            vec!["09:00".to_owned()]
        } else {
            times
        },
        once_date: task
            .schedule
            .date
            .clone()
            .filter(|date| openchamber_native_core::schedule::is_iso_date(date))
            .unwrap_or_else(|| defaults.today.clone()),
        once_time: task
            .schedule
            .time
            .clone()
            .filter(|time| openchamber_native_core::schedule::is_clock_time(time))
            .unwrap_or_else(|| "09:00".to_owned()),
        weekdays: task.schedule.weekdays.clone().unwrap_or_else(|| vec![1]),
        timezone: openchamber_native_core::time_zone::canonicalize(
            task.schedule
                .timezone
                .as_deref()
                .unwrap_or(&defaults.timezone),
        )
        .to_owned(),
        cron: if task.schedule.kind == ScheduleKind::Cron {
            task.schedule.cron.clone().unwrap_or_default()
        } else {
            String::new()
        },
        prompt: task.execution.prompt.clone(),
        provider_id: task
            .execution
            .provider_id
            .clone()
            .unwrap_or_else(|| defaults.provider_id.clone()),
        model_id: task
            .execution
            .model_id
            .clone()
            .unwrap_or_else(|| defaults.model_id.clone()),
        variant: task.execution.variant.clone().unwrap_or_default(),
        agent: task.execution.agent.clone().unwrap_or_default(),
        use_defaults: task.execution.use_defaults,
        goal_enabled: task.execution.goal_enabled,
        goal_token_budget: task.execution.goal_token_budget,
        permission_auto_accept: task.execution.permission_auto_accept,
        run_in_worktree: task.run_in_worktree,
        base_branch: task.base_branch.clone().unwrap_or_default(),
        cleanup_worktree: task.cleanup_worktree,
    }
}

impl ServerClient {
    fn scheduled_tasks_url(&self, project_id: &str, rest: &[&str]) -> Url {
        let mut url = self.endpoint().url("api/projects/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments
                .pop_if_empty()
                .push(project_id)
                .push("scheduled-tasks");
            for segment in rest {
                segments.push(segment);
            }
        }
        url
    }

    async fn scheduled_request(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> Result<(StatusCode, Option<Value>), ScheduledTaskError> {
        let mut request = self
            .authorized(method, url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TransportError::Redirected.into());
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(TransportError::AuthenticationRequired.into());
        }
        let body = response.json::<Value>().await.ok();
        Ok((status, body))
    }

    /// A list answer, or the refusal with the server's message.
    async fn task_list(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> Result<Vec<ScheduledTask>, ScheduledTaskError> {
        let (status, body) = self.scheduled_request(method, url, body).await?;
        let body = refuse_unless_success(status, body)?;
        decode_tasks(&body).ok_or_else(|| {
            TransportError::Malformed("unrecognized scheduled tasks list".to_owned()).into()
        })
    }

    /// The project's tasks.
    pub async fn scheduled_tasks(
        &self,
        project_id: &str,
    ) -> Result<Vec<ScheduledTask>, ScheduledTaskError> {
        self.task_list(Method::GET, self.scheduled_tasks_url(project_id, &[]), None)
            .await
    }

    /// Creates or replaces a task (`task` carries an `id` to replace).
    pub async fn save_scheduled_task(
        &self,
        project_id: &str,
        task: Value,
    ) -> Result<Vec<ScheduledTask>, ScheduledTaskError> {
        self.task_list(
            Method::PUT,
            self.scheduled_tasks_url(project_id, &[]),
            Some(json!({ "task": task })),
        )
        .await
    }

    pub async fn delete_scheduled_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<ScheduledTask>, ScheduledTaskError> {
        self.task_list(
            Method::DELETE,
            self.scheduled_tasks_url(project_id, &[task_id]),
            None,
        )
        .await
    }

    /// Enables or pauses a repository loop through its file.
    pub async fn set_loop_task_enabled(
        &self,
        project_id: &str,
        task_id: &str,
        enabled: bool,
    ) -> Result<(), ScheduledTaskError> {
        let url = self.scheduled_tasks_url(project_id, &[task_id, "loop-file"]);
        let (status, body) = self
            .scheduled_request(Method::PATCH, url, Some(json!({ "enabled": enabled })))
            .await?;
        refuse_unless_success(status, body).map(|_| ())
    }

    /// Deletes a repository loop's file.
    pub async fn delete_loop_task_file(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<(), ScheduledTaskError> {
        let url = self.scheduled_tasks_url(project_id, &[task_id, "loop-file"]);
        let (status, body) = self.scheduled_request(Method::DELETE, url, None).await?;
        refuse_unless_success(status, body).map(|_| ())
    }

    /// Starts a run now. A task with a run in flight or queued answers
    /// `Busy`; a malformed field of the answer is dropped on its own.
    pub async fn run_scheduled_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<RunStarted, ScheduledTaskError> {
        let url = self.scheduled_tasks_url(project_id, &[task_id, "run"]);
        let (status, body) = self.scheduled_request(Method::POST, url, None).await?;
        if status == StatusCode::CONFLICT {
            match body
                .as_ref()
                .and_then(|body| body.get("busy"))
                .and_then(Value::as_str)
            {
                Some("running") => return Err(ScheduledTaskError::Busy("running")),
                Some("queued") => return Err(ScheduledTaskError::Busy("queued")),
                _ => {}
            }
        }
        let body = refuse_unless_success(status, body)?;
        let object = body.as_object().cloned().unwrap_or_default();
        Ok(RunStarted {
            session_id: text(&object, "sessionId"),
            directory: text(&object, "directory"),
            persist_error: object
                .get("persistError")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
        })
    }

    /// Which of the candidate sessions a save or a run would accept as the
    /// task's target, by the scheduler's own rule, in batches of 500.
    pub async fn eligible_scheduled_targets(
        &self,
        project_id: &str,
        candidates: &[(String, String)],
    ) -> Result<HashSet<String>, ScheduledTaskError> {
        let mut eligible = HashSet::new();
        for batch in candidates.chunks(ELIGIBLE_BATCH) {
            let list: Vec<Value> = batch
                .iter()
                .map(|(session, directory)| json!({ "sessionId": session, "directory": directory }))
                .collect();
            let url = self.scheduled_tasks_url(project_id, &["targets", "eligible"]);
            let (status, body) = self
                .scheduled_request(Method::POST, url, Some(json!({ "candidates": list })))
                .await?;
            let body = refuse_unless_success(status, body)?;
            let entries = body
                .get("eligible")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    TransportError::Malformed("unexpected eligible targets answer".to_owned())
                })?;
            for entry in entries {
                let session = entry
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        TransportError::Malformed("unexpected eligible targets answer".to_owned())
                    })?;
                eligible.insert(session.to_owned());
            }
        }
        Ok(eligible)
    }
}

fn refuse_unless_success(
    status: StatusCode,
    body: Option<Value>,
) -> Result<Value, ScheduledTaskError> {
    if status.is_success() {
        return body.ok_or_else(|| {
            TransportError::Malformed("the scheduled tasks answer is not JSON".to_owned()).into()
        });
    }
    Err(ScheduledTaskError::Refused {
        status: status.as_u16(),
        message: body
            .as_ref()
            .and_then(|body| body.get("error"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::schedule::DraftDefaults;

    fn stored() -> Value {
        json!({
            "id": "t1", "name": "Digest", "enabled": true, "future": {"kept": 1},
            "schedule": {"kind": "weekly", "times": ["17:00", "09:00"], "weekdays": [1, 5], "timezone": "Europe/Kiev"},
            "execution": {"prompt": "Summarize", "providerID": "anthropic", "modelID": "claude", "goalEnabled": true, "goalTokenBudget": 200000},
            "state": {"createdAt": 1, "updatedAt": 2, "lastStatus": "failed", "lastError": "boom", "nextRunAt": 99}
        })
    }

    #[test]
    fn tasks_decode_and_keep_their_raw_fields() {
        let tasks = decode_tasks(&json!({"tasks": [stored(), {"id": "", "name": "x"}, 5]}))
            .expect("a list");
        assert_eq!(tasks.len(), 1, "malformed entries are dropped one by one");
        let task = &tasks[0];
        assert_eq!(task.schedule.kind, ScheduleKind::Weekly);
        assert_eq!(task.schedule.weekdays.as_deref(), Some(&[1, 5][..]));
        assert_eq!(task.state.last_status, RunStatus::Failed);
        assert_eq!(task.state.next_run_at, Some(99));
        assert!(task.cleanup_worktree, "absent means the worktree is removed");
        assert_eq!(task.raw["future"], json!({"kept": 1}));
        assert!(decode_tasks(&json!({"tasks": "no"})).is_none());
    }

    #[test]
    fn a_draft_round_trips_to_the_web_payload() {
        let tasks = decode_tasks(&json!({"tasks": [stored()]})).expect("a list");
        let defaults = DraftDefaults {
            timezone: "UTC".into(),
            today: "2026-11-07".into(),
            ..DraftDefaults::default()
        };
        let draft = task_draft(&tasks[0], &defaults);
        assert_eq!(draft.timezone, "Europe/Kyiv");
        assert_eq!(draft.times, ["09:00", "17:00"]);
        assert_eq!(draft.goal_token_budget, Some(200_000));
        let state = json!({"createdAt": 1});
        let payload = task_payload(&draft, Some(&state));
        assert_eq!(
            payload,
            json!({
                "id": "t1", "name": "Digest", "targetSessionId": "", "enabled": true,
                "schedule": {"kind": "weekly", "timezone": "Europe/Kyiv", "times": ["09:00", "17:00"], "weekdays": [1, 5]},
                "execution": {"prompt": "Summarize", "providerID": "anthropic", "modelID": "claude", "goalEnabled": true, "goalTokenBudget": 200000},
                "runInWorktree": false, "baseBranch": "", "cleanupWorktree": true,
                "state": {"createdAt": 1}
            })
        );
        let existing = TaskDraft {
            session_mode: SessionMode::Existing,
            target_session_id: "ses_1".into(),
            kind: ScheduleKind::Cron,
            cron: " 0 9 * * 1 ".into(),
            ..draft
        };
        let payload = task_payload(&existing, Some(&state));
        assert_eq!(payload["targetSessionId"], json!("ses_1"));
        assert_eq!(payload["schedule"], json!({"kind": "cron", "timezone": "Europe/Kyiv", "cron": "0 9 * * 1"}));
        assert!(payload.get("runInWorktree").is_none() && payload.get("state").is_none());
    }

    #[test]
    fn refusals_carry_the_server_message() {
        let error = refuse_unless_success(
            StatusCode::BAD_REQUEST,
            Some(json!({"error": " Invalid cron expression "})),
        )
        .expect_err("refused");
        assert_eq!(error.message(), Some("Invalid cron expression"));
        assert!(refuse_unless_success(StatusCode::OK, None).is_err());
    }
}
