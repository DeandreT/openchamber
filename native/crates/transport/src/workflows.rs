//! Workflow runs, owned by the OpenChamber server (`lib/workflows`): the
//! runs a session proposed (`GET /api/openchamber/workflows?sessionId=`) and
//! the user's approve, decline, stop or resume
//! (`POST /api/openchamber/workflows/:runId/:action`). A run's new state
//! arrives as an `openchamber:workflow-run` control event, never in the
//! action's answer.
//!
//! Mirrors `packages/ui/src/lib/workflows/api.ts` and the schemas of
//! `packages/ui/src/lib/workflows/model.ts`: a list with one unrecognized
//! run is a failed read, never a shorter list.

use openchamber_native_core::ids::SessionId;
use openchamber_native_core::workflow_runs::{
    WorkflowAgent, WorkflowAgentState, WorkflowRun, WorkflowRunAction, WorkflowRunId,
    WorkflowRunState,
};
use reqwest::{Method, Url};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::files::read_answer;

/// Why a run action did not go through.
#[derive(Debug, Error)]
pub enum WorkflowActionError {
    /// The server answered and refused; `message` is the reason it gave.
    #[error("the workflow did not accept the action (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

fn malformed() -> TransportError {
    TransportError::Malformed("unrecognized workflow runs".to_owned())
}

/// A key that may be absent but, when present, must satisfy `read`
/// (`z.optional()`). The outer `None` is a malformed value.
fn optional<'value, T>(
    object: &'value Map<String, Value>,
    key: &str,
    read: impl FnOnce(&'value Value) -> Option<T>,
) -> Option<Option<T>> {
    match object.get(key) {
        None => Some(None),
        Some(value) => read(value).map(Some),
    }
}

/// A key that must be present as a string or `null` (`z.string().nullable()`).
fn nullable_text(object: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match object.get(key)? {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(text.clone())),
        _ => None,
    }
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn agent(value: &Value) -> Option<WorkflowAgent> {
    let object = value.as_object()?;
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    // Fields the card does not show still decide whether the summary is one
    // this client understands.
    for key in ["startedAt", "endedAt"] {
        optional(object, key, Value::as_f64)?;
    }
    optional(object, "model", Value::as_str)?;
    optional(object, "usage", |usage| {
        let usage = usage.as_object()?;
        [
            "cost",
            "input",
            "output",
            "reasoning",
            "cacheRead",
            "cacheWrite",
        ]
        .iter()
        .all(|key| usage.get(*key).is_some_and(Value::is_number))
        .then_some(())
    })?;
    Some(WorkflowAgent {
        key: object.get("key").and_then(non_empty)?.to_owned(),
        label: text("label")?.to_owned(),
        phase: nullable_text(object, "phase")?,
        state: WorkflowAgentState::from_wire(text("state")?)?,
        session_id: optional(object, "sessionId", non_empty)?.map(SessionId::from),
        replayed: optional(object, "replayed", Value::as_bool)?.unwrap_or(false),
        error: optional(object, "error", Value::as_str)?.map(str::to_owned),
        preview: optional(object, "preview", Value::as_str)?.map(str::to_owned),
    })
}

/// Decodes one run summary (`workflowRunSchema`); `None` for a malformed one.
pub(crate) fn decode_run(value: &Value) -> Option<WorkflowRun> {
    let object = value.as_object()?;
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    let created_at = object
        .get("createdAt")
        .and_then(Value::as_f64)
        .filter(|created_at| created_at.is_finite())?;
    object
        .get("attempt")
        .filter(|attempt| attempt.is_number())?;
    for key in ["startedAt", "endedAt"] {
        optional(object, key, Value::as_f64)?;
    }
    optional(object, "resultPreview", Value::as_str)?;
    let logs = object.get("logs").and_then(Value::as_array)?;
    if !logs.iter().all(|line| {
        line.get("at").is_some_and(Value::is_number)
            && line.get("message").is_some_and(Value::is_string)
    }) {
        return None;
    }
    let phases = object
        .get("phases")
        .and_then(Value::as_array)?
        .iter()
        .map(|phase| phase.as_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()?;
    let agents = object
        .get("agents")
        .and_then(Value::as_array)?
        .iter()
        .map(agent)
        .collect::<Option<Vec<WorkflowAgent>>>()?;
    Some(WorkflowRun {
        id: WorkflowRunId::from(object.get("id").and_then(non_empty)?),
        parent_session_id: SessionId::from(object.get("parentSessionId").and_then(non_empty)?),
        directory: text("directory")?.to_owned(),
        name: text("name")?.to_owned(),
        description: text("description")?.to_owned(),
        state: WorkflowRunState::from_wire(text("state")?)?,
        // JSON numbers keep their integer part.
        created_at: created_at as i64,
        current_phase: nullable_text(object, "currentPhase")?,
        phases,
        agents,
        error: optional(object, "error", Value::as_str)?.map(str::to_owned),
    })
}

/// Decodes `{ runs }` (`workflowRunListSchema`).
fn decode_runs(body: &Value) -> Option<Vec<WorkflowRun>> {
    body.get("runs")?
        .as_array()?
        .iter()
        .map(decode_run)
        .collect()
}

/// Decodes an `openchamber:workflow-run-removed` control event: the deleted
/// run and the session that proposed it.
pub(crate) fn decode_removed(properties: &Value) -> Option<(SessionId, WorkflowRunId)> {
    let text = |key: &str| properties.get(key).and_then(non_empty);
    Some((
        SessionId::from(text("parentSessionId")?),
        WorkflowRunId::from(text("runId")?),
    ))
}

impl ServerClient {
    /// The URL of the runs `session` proposed.
    pub fn workflow_runs_url(&self, session: &SessionId) -> Url {
        let mut url = self.endpoint().url("api/openchamber/workflows");
        url.query_pairs_mut()
            .append_pair("sessionId", session.as_str());
        url
    }

    /// The URL of one action on `run`.
    pub fn workflow_run_action_url(&self, run: &WorkflowRunId, action: WorkflowRunAction) -> Url {
        let mut url = self.endpoint().url("api/openchamber/workflows/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(run.as_str())
            .push(action.as_str());
        url
    }

    /// The workflow runs `session` proposed, oldest first. An answer this
    /// client cannot read is an error, never an empty list.
    pub async fn workflow_runs(
        &self,
        session: &SessionId,
    ) -> Result<Vec<WorkflowRun>, TransportError> {
        let body: Value = self.get_json(self.workflow_runs_url(session), None).await?;
        decode_runs(&body).ok_or_else(malformed)
    }

    /// Sends the user's approve, decline, stop or resume for `run`. The
    /// run's new state arrives as a control event.
    pub async fn workflow_run_action(
        &self,
        run: &WorkflowRunId,
        action: WorkflowRunAction,
    ) -> Result<(), WorkflowActionError> {
        let response = self
            .authorized(Method::POST, self.workflow_run_action_url(run, action))
            .json(&json!({}))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(WorkflowActionError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message.filter(|message| !message.is_empty()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> Value {
        json!({
            "id": id,
            "parentSessionId": "ses_parent",
            "directory": "/repo",
            "name": "Audit",
            "description": "Checks every module",
            "state": "running",
            "attempt": 1,
            "createdAt": 1_700_000_000_123.0_f64,
            "startedAt": 1_700_000_000_500.0_f64,
            "currentPhase": "scan",
            "phases": ["scan", "report"],
            "agents": [
                {
                    "key": "scan-1", "label": "Scan core", "phase": "scan", "state": "completed",
                    "sessionId": "ses_agent", "model": "p/m",
                    "usage": {"cost": 0.1, "input": 1, "output": 2, "reasoning": 0, "cacheRead": 0, "cacheWrite": 0},
                    "preview": "done"
                },
                {"key": "late", "label": "Late", "phase": null, "state": "queued", "replayed": true}
            ],
            "logs": [{"at": 1, "message": "started"}]
        })
    }

    #[test]
    fn a_summary_decodes_into_the_fields_the_card_reads() {
        let run = decode_run(&summary("run_1")).expect("valid summary");
        assert_eq!(run.id, WorkflowRunId::from("run_1"));
        assert_eq!(run.parent_session_id, SessionId::from("ses_parent"));
        assert_eq!(run.state, WorkflowRunState::Running);
        assert_eq!(run.created_at, 1_700_000_000_123);
        assert_eq!(run.current_phase.as_deref(), Some("scan"));
        assert_eq!(run.phases, ["scan", "report"]);
        assert_eq!(run.error, None);
        assert_eq!(run.agents[0].session_id, Some(SessionId::from("ses_agent")));
        assert_eq!(run.agents[0].preview.as_deref(), Some("done"));
        assert!(!run.agents[0].replayed);
        assert_eq!(run.agents[1].phase, None);
        assert_eq!(run.agents[1].session_id, None);
        assert!(run.agents[1].replayed);
        assert_eq!(run.progress().as_deref(), Some("scan 1/1"));
    }

    #[test]
    fn a_summary_outside_the_schema_is_not_a_run() {
        let broken = |change: fn(&mut Value)| {
            let mut value = summary("run_1");
            change(&mut value);
            decode_run(&value)
        };
        assert!(broken(|value| value["id"] = json!("")).is_none());
        assert!(broken(|value| value["state"] = json!("paused")).is_none());
        assert!(
            broken(|value| {
                value
                    .as_object_mut()
                    .expect("object")
                    .remove("currentPhase");
            })
            .is_none(),
            "a nullable key must still be present"
        );
        assert!(broken(|value| value["currentPhase"] = Value::Null).is_some());
        assert!(broken(|value| value["agents"][0]["state"] = json!("asleep")).is_none());
        assert!(broken(|value| value["agents"][1]["sessionId"] = json!("")).is_none());
        assert!(broken(|value| value["agents"][0]["usage"] = json!({"cost": 1})).is_none());
        assert!(broken(|value| value["logs"] = json!([{"at": 1}])).is_none());
        assert!(broken(|value| value["error"] = json!(7)).is_none());
        assert!(broken(|value| value["error"] = json!("boom")).is_some());
    }

    #[test]
    fn removal_events_name_the_run_and_its_session() {
        assert_eq!(
            decode_removed(&json!({"runId": "run_1", "parentSessionId": "ses_parent"})),
            Some((SessionId::from("ses_parent"), WorkflowRunId::from("run_1")))
        );
        assert_eq!(decode_removed(&json!({"runId": "run_1"})), None);
        assert_eq!(
            decode_removed(&json!({"runId": "", "parentSessionId": "ses_parent"})),
            None
        );
    }

    #[tokio::test]
    async fn runs_are_read_by_session_and_one_unreadable_run_fails_the_read() {
        let (client, recorded) =
            crate::test_server::serve(Box::new(|request| match request.path.as_str() {
                "/api/openchamber/workflows" => (200, json!({"runs": [summary("run_1")]})),
                _ => (404, json!({"error": "unknown"})),
            }))
            .await;
        let session = SessionId::from("ses a/b");
        assert_eq!(
            client.workflow_runs_url(&session).query(),
            Some("sessionId=ses+a%2Fb"),
            "the session travels as a query value"
        );
        let runs = client.workflow_runs(&session).await.expect("runs");
        assert_eq!(runs.len(), 1);
        {
            let recorded = recorded.lock().expect("log");
            assert_eq!(recorded[0].method, "GET");
            assert_eq!(recorded[0].path, "/api/openchamber/workflows");
        }

        let (client, _) = crate::test_server::serve(Box::new(|_| {
            let mut broken = summary("run_2");
            broken["state"] = json!("paused");
            (200, json!({"runs": [summary("run_1"), broken]}))
        }))
        .await;
        assert!(
            matches!(
                client.workflow_runs(&session).await,
                Err(TransportError::Malformed(_))
            ),
            "a list with an unknown run is not a shorter list"
        );

        let (client, _) = crate::test_server::serve(Box::new(|_| (200, json!({"runs": []})))).await;
        assert!(
            client
                .workflow_runs(&session)
                .await
                .expect("empty")
                .is_empty()
        );

        let (client, _) =
            crate::test_server::serve(Box::new(|_| (503, json!({"error": "starting"})))).await;
        assert!(matches!(
            client.workflow_runs(&session).await,
            Err(TransportError::Http { status: 503 })
        ));
    }

    #[tokio::test]
    async fn an_action_posts_to_its_run_and_a_refusal_keeps_the_servers_reason() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            if request.path.ends_with("/resume") {
                (409, json!({"error": "The run is already going"}))
            } else if request.path.ends_with("/stop") {
                (500, json!({}))
            } else {
                (200, json!({"ok": true}))
            }
        }))
        .await;
        let run = WorkflowRunId::from("run 1/x");
        client
            .workflow_run_action(&run, WorkflowRunAction::Approve)
            .await
            .expect("approved");
        match client
            .workflow_run_action(&run, WorkflowRunAction::Resume)
            .await
        {
            Err(WorkflowActionError::Refused { status, message }) => {
                assert_eq!(status, 409);
                assert_eq!(message.as_deref(), Some("The run is already going"));
            }
            other => panic!("a refusal must carry the server's reason, got {other:?}"),
        }
        assert!(matches!(
            client
                .workflow_run_action(&run, WorkflowRunAction::Stop)
                .await,
            Err(WorkflowActionError::Refused {
                status: 500,
                message: None
            })
        ));
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(
            recorded[0].path, "/api/openchamber/workflows/run%201%2Fx/approve",
            "the run id is one path segment"
        );
        assert_eq!(recorded[0].body, json!({}));
        assert_eq!(
            recorded[1].path,
            "/api/openchamber/workflows/run%201%2Fx/resume"
        );
    }
}
