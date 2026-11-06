//! The session goal's writes and reads, as `lib/sessionGoalActions.ts`,
//! `lib/goalObjectiveFiles.ts` and `stores/useSmallModelStore.ts` make them.
//!
//! The goal is `metadata.openchamber.goal`, merge-patched through the
//! OpenChamber metadata route like every other overlay key. Its objective
//! text lives in a server-side file keyed by the session
//! (`/api/goals/objective/:sessionId`); the metadata carries only
//! `objectiveFile: true`, so multi-kilobyte objectives stay out of every
//! `session.updated`. The file is written before the metadata, and a failed
//! file write falls back to the inline objective. Nothing here retries.

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::session::MetadataFacts;
use openchamber_native_core::session_goal::{self, GoalStatus, SessionGoal};
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;
use crate::prompt_parts::SyntheticContext;
use crate::session_actions::{namespace, with_namespace};

/// A new or edited goal (`SetSessionGoalInput`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalInput {
    /// The objective as fitted for the audit, already trimmed.
    pub objective: String,
    pub token_budget: Option<u64>,
}

/// What the status buttons may set (`setSessionGoalStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalStatusChange {
    /// Resume: the server's kickoff signal, with a fresh turn allowance.
    Active,
    /// Pause, which also stops the running turn.
    Paused,
}

#[derive(Deserialize)]
struct ObjectiveResponse {
    content: String,
}

#[derive(Deserialize)]
struct SmallModelResponse {
    available: Option<Value>,
}

impl ServerClient {
    fn objective_url(&self, session: &SessionId) -> Url {
        let mut url = self.endpoint().url("api/goals/objective/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(session.as_str());
        url
    }

    pub(crate) fn write_objective_request(
        &self,
        session: &SessionId,
        content: &str,
    ) -> crate::request::RequestBuilder {
        self.authorized(Method::PUT, self.objective_url(session))
            .json(&json!({ "content": content }))
    }

    /// Writes the session's objective file.
    pub async fn write_goal_objective(
        &self,
        session: &SessionId,
        content: &str,
    ) -> Result<(), TransportError> {
        let response = self
            .write_objective_request(session, content)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let _: Value = read_json(response).await?;
        Ok(())
    }

    /// The file-backed objective text. `Ok(None)` is the server's 404: the
    /// file is gone, which the display tells apart from a failed read.
    pub async fn goal_objective(
        &self,
        session: &SessionId,
    ) -> Result<Option<String>, TransportError> {
        let response = self
            .authorized(Method::GET, self.objective_url(session))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let reply: ObjectiveResponse = read_json(response).await?;
        Ok(Some(reply.content))
    }

    /// Deletes the objective file. A failure leaves an orphan the next goal
    /// overwrites, so callers ignore it.
    pub async fn delete_goal_objective(&self, session: &SessionId) -> Result<(), TransportError> {
        let response = self
            .authorized(Method::DELETE, self.objective_url(session))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let _: Value = read_json(response).await?;
        Ok(())
    }

    /// `setSessionGoal`: writes the objective file, then the goal. A goal
    /// whose id still matches `existing` (and is not complete) is edited in
    /// place, keeping its accounting; anything else starts a new goal.
    pub async fn set_session_goal(
        &self,
        session: &SessionId,
        input: &GoalInput,
        existing: Option<&SessionGoal>,
        new_id: &str,
        now: i64,
    ) -> Result<MetadataFacts, TransportError> {
        let objective_file = self
            .write_goal_objective(session, &input.objective)
            .await
            .is_ok();
        self.patch_metadata(session, |metadata| {
            with_goal(metadata, input, objective_file, existing, new_id, now)
        })
        .await
    }

    /// `setSessionGoalStatus` for Pause and Resume. Pause also stops the
    /// running turn, beside the write and whatever its outcome.
    pub async fn set_session_goal_status(
        &self,
        session: &SessionId,
        status: GoalStatusChange,
        now: i64,
    ) -> Result<MetadataFacts, TransportError> {
        let write =
            self.patch_metadata(session, |metadata| with_goal_status(metadata, status, now));
        if status != GoalStatusChange::Paused {
            return write.await;
        }
        let (_, written) = futures_util::future::join(self.abort_session(session), write).await;
        written
    }

    /// `clearSessionGoal`: removes the goal, deletes its file and, when the
    /// goal was running, stops the turn.
    pub async fn clear_session_goal(
        &self,
        session: &SessionId,
    ) -> Result<MetadataFacts, TransportError> {
        let mut was_active = false;
        let facts = self
            .patch_metadata(session, |metadata| {
                let (next, active) = without_goal(metadata);
                was_active = active;
                next
            })
            .await?;
        let _ = self.delete_goal_objective(session).await;
        if was_active {
            let _ = self.abort_session(session).await;
        }
        Ok(facts)
    }

    /// Admits the goal's first-turn reminder as a synthetic message that
    /// starts no turn, under `id`. OpenCode answers 409 to a second admission
    /// of an id, which counts as admitted.
    pub async fn admit_goal_intro(
        &self,
        session: &SessionId,
        id: &MessageId,
        token_budget: Option<u64>,
    ) -> Result<(), TransportError> {
        let context = goal_intro_context(id.clone(), token_budget);
        let response = self
            .synthetic_request(session, &context, None)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        match response.status() {
            StatusCode::OK | StatusCode::CONFLICT => Ok(()),
            status if status.is_redirection() => Err(TransportError::Redirected),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(TransportError::AuthenticationRequired)
            }
            status => Err(TransportError::Http {
                status: status.as_u16(),
            }),
        }
    }

    /// `GET /api/small-model`: whether the background model can run for
    /// `directory`. `Ok(None)` is an answer without a usable `available`,
    /// which, like a failure, says nothing about the model.
    pub async fn small_model_available(
        &self,
        directory: Option<&str>,
    ) -> Result<Option<bool>, TransportError> {
        let mut url = self.endpoint().url("api/small-model");
        if let Some(directory) = directory.map(str::trim).filter(|value| !value.is_empty()) {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        let response = self
            .authorized(Method::GET, url)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: SmallModelResponse = read_json(response).await?;
        Ok(reply.available.as_ref().and_then(Value::as_bool))
    }
}

/// The armed send's first-turn reminder as a context item.
pub fn goal_intro_context(id: MessageId, token_budget: Option<u64>) -> SyntheticContext {
    SyntheticContext {
        id,
        text: session_goal::intro(token_budget),
        description: None,
        metadata: None,
    }
}

/// `createGoalId`'s shape: the time in base 36 and six random characters.
pub fn new_goal_id(now: i64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i64(now);
    format!(
        "{}{}",
        base36(now.max(0) as u64),
        &format!("{:0>6}", base36(hasher.finish() % 36_u64.pow(6)))
    )
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("base-36 digits are ASCII")
}

fn current_goal(metadata: &Map<String, Value>) -> Option<Map<String, Value>> {
    namespace(metadata)
        .get("goal")
        .and_then(Value::as_object)
        .cloned()
}

fn with_goal_value(metadata: &Map<String, Value>, goal: Option<Value>) -> Map<String, Value> {
    let mut space = namespace(metadata);
    match goal {
        Some(goal) => {
            space.insert("goal".to_owned(), goal);
        }
        None => {
            space.remove("goal");
        }
    }
    with_namespace(metadata, space)
}

/// The goal `setSessionGoal` writes.
fn with_goal(
    metadata: &Map<String, Value>,
    input: &GoalInput,
    objective_file: bool,
    existing: Option<&SessionGoal>,
    new_id: &str,
    now: i64,
) -> Map<String, Value> {
    let objective = if objective_file {
        ""
    } else {
        input.objective.as_str()
    };
    let current = current_goal(metadata);
    let edit = existing.filter(|existing| {
        existing.status != GoalStatus::Complete
            && current
                .as_ref()
                .and_then(|goal| goal.get("id"))
                .and_then(Value::as_str)
                == Some(existing.id.as_str())
    });
    let goal = match (edit, current) {
        (Some(_), Some(mut goal)) => {
            // Edit in place: keep accounting, reactivate, clear stale audit state.
            goal.insert("objective".to_owned(), json!(objective));
            goal.insert("objectiveFile".to_owned(), json!(objective_file));
            goal.insert("tokenBudget".to_owned(), json!(input.token_budget));
            goal.insert("status".to_owned(), json!("active"));
            goal.insert("statusReason".to_owned(), json!("resumed"));
            goal.insert("blockedStreak".to_owned(), json!(0));
            goal.insert("updatedAt".to_owned(), json!(now));
            Value::Object(goal)
        }
        _ => json!({
            "id": new_id,
            "objective": objective,
            "objectiveFile": objective_file,
            "status": "active",
            "tokenBudget": input.token_budget,
            "tokensUsed": 0,
            "turnsUsed": 0,
            "blockedStreak": 0,
            "note": "",
            "statusReason": "",
            "lastAccountedMessageID": "",
            "createdAt": now,
            "updatedAt": now,
        }),
    };
    with_goal_value(metadata, Some(goal))
}

/// The goal after Pause or Resume; no goal stays no goal.
fn with_goal_status(
    metadata: &Map<String, Value>,
    status: GoalStatusChange,
    now: i64,
) -> Map<String, Value> {
    let Some(mut goal) = current_goal(metadata) else {
        return with_goal_value(metadata, None);
    };
    let (wire, reason) = match status {
        GoalStatusChange::Active => ("active", "resumed"),
        GoalStatusChange::Paused => ("paused", ""),
    };
    goal.insert("status".to_owned(), json!(wire));
    goal.insert("statusReason".to_owned(), json!(reason));
    goal.insert("blockedStreak".to_owned(), json!(0));
    // An explicit resume grants a fresh auto-continuation allowance, or a
    // goal blocked on the turn cap would block again on the next tick.
    if status == GoalStatusChange::Active {
        goal.insert("turnsUsed".to_owned(), json!(0));
    }
    goal.insert("updatedAt".to_owned(), json!(now));
    with_goal_value(metadata, Some(Value::Object(goal)))
}

/// The metadata without its goal, and whether that goal was running.
fn without_goal(metadata: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let active = current_goal(metadata)
        .and_then(|goal| goal.get("status").cloned())
        .and_then(|status| status.as_str().map(|status| status == "active"))
        == Some(true);
    (with_goal_value(metadata, None), active)
}

/// The goal in a record read back, as the projection parses it.
pub fn goal_of(metadata: &Map<String, Value>) -> Option<SessionGoal> {
    crate::projection::metadata_facts(Some(&Value::Object(metadata.clone()))).goal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::Credential;
    use crate::endpoint::Endpoint;
    use crate::session_actions::merge_patch;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn input(objective: &str, budget: Option<u64>) -> GoalInput {
        GoalInput {
            objective: objective.to_owned(),
            token_budget: budget,
        }
    }

    #[test]
    fn a_new_goal_starts_fresh_and_keeps_other_keys() {
        let current = object(json!({ "openchamber": { "pinned": { "at": 1 } }, "x": 1 }));
        let next = with_goal(&current, &input("Ship it", None), true, None, "g1", 7);
        assert_eq!(
            Value::Object(merge_patch(&current, &next)),
            json!({ "openchamber": { "goal": {
                "id": "g1", "objective": "", "objectiveFile": true, "status": "active",
                "tokenBudget": null, "tokensUsed": 0, "turnsUsed": 0, "blockedStreak": 0,
                "note": "", "statusReason": "", "lastAccountedMessageID": "",
                "createdAt": 7, "updatedAt": 7
            } } })
        );
        let inline = with_goal(
            &current,
            &input("Ship it", Some(5000)),
            false,
            None,
            "g1",
            7,
        );
        let goal = goal_of(&inline).expect("goal");
        assert_eq!(goal.inline_objective(), Some("Ship it"));
        assert_eq!(goal.token_budget, Some(5000));
    }

    #[test]
    fn an_edit_keeps_accounting_only_for_the_same_live_goal() {
        let current = object(json!({ "openchamber": { "goal": {
            "id": "g1", "objective": "Old", "status": "blocked", "tokensUsed": 900,
            "turnsUsed": 4, "createdAt": 1, "updatedAt": 2
        } } }));
        let existing = goal_of(&current).expect("goal");
        let edited = with_goal(
            &current,
            &input("New", Some(2000)),
            false,
            Some(&existing),
            "g2",
            9,
        );
        let goal = goal_of(&edited).expect("goal");
        assert_eq!((goal.id.as_str(), goal.tokens_used), ("g1", 900));
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.status_reason, "resumed");
        // The goal changed under the dialog: a new goal, fresh accounting.
        let replaced = object(json!({ "openchamber": { "goal": {
            "id": "other", "objective": "Old", "status": "active", "tokensUsed": 50
        } } }));
        let fresh = goal_of(&with_goal(
            &replaced,
            &input("New", None),
            false,
            Some(&existing),
            "g2",
            9,
        ))
        .expect("goal");
        assert_eq!((fresh.id.as_str(), fresh.tokens_used), ("g2", 0));
    }

    #[test]
    fn resume_resets_the_turn_allowance_and_pause_keeps_it() {
        let current = object(json!({ "openchamber": { "goal": {
            "id": "g1", "objective": "Old", "status": "blocked", "turnsUsed": 20,
            "statusReason": "turn limit"
        } } }));
        assert_eq!(
            Value::Object(merge_patch(
                &current,
                &with_goal_status(&current, GoalStatusChange::Active, 5)
            )),
            json!({ "openchamber": { "goal": {
                "status": "active", "statusReason": "resumed", "blockedStreak": 0,
                "turnsUsed": 0, "updatedAt": 5
            } } })
        );
        assert_eq!(
            Value::Object(merge_patch(
                &current,
                &with_goal_status(&current, GoalStatusChange::Paused, 5)
            )),
            json!({ "openchamber": { "goal": {
                "status": "paused", "statusReason": "", "blockedStreak": 0, "updatedAt": 5
            } } })
        );
        let empty = object(json!({ "openchamber": { "pinned": { "at": 1 } } }));
        assert!(
            merge_patch(
                &empty,
                &with_goal_status(&empty, GoalStatusChange::Active, 5)
            )
            .is_empty()
        );
    }

    #[test]
    fn clearing_reports_whether_the_goal_was_running() {
        let running =
            object(json!({ "openchamber": { "goal": { "id": "g", "status": "active" } } }));
        let (next, active) = without_goal(&running);
        assert!(active);
        assert_eq!(
            Value::Object(merge_patch(&running, &next)),
            json!({ "openchamber": { "goal": null } })
        );
        let paused =
            object(json!({ "openchamber": { "goal": { "id": "g", "status": "paused" } } }));
        assert!(!without_goal(&paused).1);
    }

    #[test]
    fn requests_match_the_web_client() {
        let client = ServerClient::new(
            Endpoint::parse("http://127.0.0.1:4799").expect("endpoint"),
            Credential::None,
        )
        .expect("client");
        let request = client
            .write_objective_request(&SessionId::new("ses_a"), "Ship it")
            .build()
            .expect("request");
        assert_eq!(request.method().as_str(), "PUT");
        assert_eq!(request.url().path(), "/api/goals/objective/ses_a");
        let body: Value = serde_json::from_slice(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("body"),
        )
        .expect("JSON");
        assert_eq!(body, json!({ "content": "Ship it" }));
    }

    #[test]
    fn goal_ids_have_the_reference_shape() {
        let id = new_goal_id(1_700_000_000_000);
        assert!(id.starts_with(&base36(1_700_000_000_000)));
        assert_eq!(id.len(), base36(1_700_000_000_000).len() + 6);
        assert!(id.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    fn the_projection_rejects_invalid_goals() {
        for goal in [
            json!({ "id": "", "objective": "x", "status": "active" }),
            json!({ "id": "g", "objective": "  ", "status": "active" }),
            json!({ "id": "g", "objective": "x", "status": "done" }),
        ] {
            assert_eq!(
                goal_of(&object(json!({ "openchamber": { "goal": goal } }))),
                None
            );
        }
        let goal = goal_of(&object(json!({ "openchamber": { "goal": {
            "id": "g", "objectiveFile": true, "status": "budgetLimited",
            "tokenBudget": 1500.7, "tokensUsed": -3, "turnsUsed": 2.9
        } } })))
        .expect("goal");
        assert_eq!(goal.token_budget, Some(1500));
        assert_eq!((goal.tokens_used, goal.turns_used), (0, 2));
        assert_eq!(goal.inline_objective(), None);
    }
}
