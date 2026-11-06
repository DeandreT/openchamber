//! The review flow's server calls: the auto-review routes
//! (`web/server/lib/auto-review`), as `stores/useAutoReviewStore.ts` reads
//! and changes them, and the review session `lib/reviewFlow.ts` creates or
//! reuses before a run starts.
//!
//! A run belongs to the server that sees its sessions' OpenCode: the host's,
//! or an isolated space's for a directory inside one, which the request names
//! in the `x-opencode-directory` header. Every answer carries the owner's
//! revision; `openchamber_native_core::auto_review` applies it. Nothing here
//! retries.

use openchamber_native_core::auto_review::{AutoReviewRun, RunStatus, RunStep, owner_of};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_core::session::SessionRecord;
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory, read_json};
use crate::error::TransportError;
use crate::prompt::send_checked;
use crate::request::RequestBuilder;
use crate::session_actions::{metadata_object, namespace, with_namespace};
use crate::session_create::{CreateSession, SessionError};

/// One owner's runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoReviewSnapshot {
    pub revision: u64,
    pub runs: Vec<AutoReviewRun>,
}

/// One run after a start or a removal; `None` once removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoReviewMutation {
    pub revision: u64,
    pub run: Option<AutoReviewRun>,
}

/// The prompts the server sends, rendered from the magic prompts with their
/// `{{handoff}}`, `{{review_feedback}}` and `{{implementation_response}}`
/// placeholders kept for the server to fill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReviewPrompts {
    pub review_start: String,
    pub review_rules: String,
    pub feedback_to_implementer: String,
    pub response_to_reviewer: String,
}

/// The handoff request the run sends the implementer first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutoReviewHandoff {
    pub text: String,
    pub instructions: String,
}

/// The review session a review runs in.
#[derive(Clone, Debug)]
pub struct ReviewSession {
    pub record: SessionRecord,
    /// Made for this review, so it still needs the reviewed session's
    /// permission mode; a reused one keeps its own.
    pub created: bool,
}

/// `POST /api/auto-review/runs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoReviewStart {
    pub original_session: SessionId,
    pub review_session: SessionId,
    pub directory: String,
    pub handoff: Option<AutoReviewHandoff>,
    pub prompts: AutoReviewPrompts,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartBody<'a> {
    #[serde(rename = "originalSessionID")]
    original_session_id: &'a str,
    #[serde(rename = "reviewSessionID")]
    review_session_id: &'a str,
    directory: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    handoff: Option<&'a AutoReviewHandoff>,
    prompts: &'a AutoReviewPrompts,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunWire {
    #[serde(rename = "originalSessionID")]
    original_session_id: String,
    #[serde(rename = "reviewSessionID")]
    review_session_id: String,
    directory: String,
    status: String,
    step: String,
    round: f64,
    max_rounds: f64,
    started_at: f64,
    updated_at: f64,
    error: Option<String>,
    dispute: Option<String>,
}

#[derive(Deserialize)]
struct SnapshotWire {
    revision: f64,
    runs: Vec<RunWire>,
}

#[derive(Deserialize)]
struct MutationWire {
    revision: f64,
    run: Option<RunWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateWire {
    revision: f64,
    #[serde(rename = "originalSessionID")]
    original_session_id: String,
    directory: String,
    run: Option<RunWire>,
    space_id: Option<String>,
}

/// `openchamber:auto-review.updated`: one run's state from its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoReviewUpdate {
    /// The space whose own server forwarded the change, else the host.
    pub space: Option<String>,
    pub revision: u64,
    pub original_session: SessionId,
    pub directory: String,
    pub run: Option<AutoReviewRun>,
}

fn revision(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

/// `runSchema`: every field present and valid, or the run is malformed.
fn run(wire: RunWire) -> Option<AutoReviewRun> {
    if wire.original_session_id.is_empty()
        || wire.review_session_id.is_empty()
        || wire.directory.is_empty()
    {
        return None;
    }
    Some(AutoReviewRun {
        original_session: SessionId::new(wire.original_session_id),
        review_session: SessionId::new(wire.review_session_id),
        directory: wire.directory,
        status: RunStatus::from_wire(&wire.status)?,
        step: RunStep::from_wire(&wire.step)?,
        round: wire.round.max(0.0) as u32,
        max_rounds: wire.max_rounds.max(0.0) as u32,
        started_at: wire.started_at as i64,
        updated_at: wire.updated_at as i64,
        error: wire.error,
        dispute: wire.dispute,
    })
}

fn malformed() -> TransportError {
    TransportError::Malformed("invalid auto-review answer".to_owned())
}

/// Decodes a broadcast; `None` for a malformed one, which changes nothing.
pub(crate) fn decode_update(properties: &Value) -> Option<AutoReviewUpdate> {
    let wire: UpdateWire = serde_json::from_value(properties.clone()).ok()?;
    if wire.original_session_id.is_empty() || wire.directory.is_empty() {
        return None;
    }
    let run = match wire.run {
        Some(wire) => Some(run(wire)?),
        None => None,
    };
    Some(AutoReviewUpdate {
        space: wire.space_id,
        revision: revision(wire.revision)?,
        original_session: SessionId::new(wire.original_session_id),
        directory: wire.directory,
        run,
    })
}

impl ServerClient {
    /// A request the space owning `directory` answers, or the host.
    fn owned_request(&self, method: Method, url: Url, directory: &str) -> RequestBuilder {
        let request = self.authorized(method, url);
        if owner_of(directory).is_some() {
            request
                .header("x-opencode-directory", encode_directory(directory))
                .header("x-opencode-directory-encoding", "uri")
        } else {
            request
        }
    }

    fn run_url(&self, original: &SessionId) -> Url {
        let mut url = self.endpoint().url("api/auto-review/runs/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(original.as_str());
        url
    }

    /// One owner's runs: `None` reads the host's, a space ID that space's.
    pub async fn auto_review_runs(
        &self,
        space: Option<&str>,
    ) -> Result<AutoReviewSnapshot, TransportError> {
        let directory = space.map(|space| format!("/spaces/{space}"));
        let response = self
            .owned_request(
                Method::GET,
                self.endpoint().url("api/auto-review"),
                directory.as_deref().unwrap_or_default(),
            )
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let wire: SnapshotWire = read_json(response).await.map_err(|error| match error {
            TransportError::Malformed(_) => malformed(),
            other => other,
        })?;
        let runs = wire.runs.into_iter().map(run).collect::<Option<Vec<_>>>();
        Ok(AutoReviewSnapshot {
            revision: revision(wire.revision).ok_or_else(malformed)?,
            runs: runs.ok_or_else(malformed)?,
        })
    }

    pub(crate) fn start_auto_review_request(&self, start: &AutoReviewStart) -> RequestBuilder {
        self.owned_request(
            Method::POST,
            self.endpoint().url("api/auto-review/runs"),
            &start.directory,
        )
        .json(&StartBody {
            original_session_id: start.original_session.as_str(),
            review_session_id: start.review_session.as_str(),
            directory: &start.directory,
            handoff: start.handoff.as_ref(),
            prompts: &start.prompts,
        })
    }

    /// Starts a run; the server answers 409 while one of that session runs.
    pub async fn start_auto_review(
        &self,
        start: &AutoReviewStart,
    ) -> Result<AutoReviewMutation, TransportError> {
        let response = self
            .start_auto_review_request(start)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        mutation(response).await
    }

    /// Stops a running run or dismisses a finished one.
    pub async fn remove_auto_review(
        &self,
        original: &SessionId,
        directory: &str,
    ) -> Result<AutoReviewMutation, TransportError> {
        let response = self
            .owned_request(Method::DELETE, self.run_url(original), directory)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        mutation(response).await
    }

    /// `createOrReuseReviewSession`: the session's linked review session when
    /// it still is one; otherwise a stale link goes, and a new review session
    /// `id` is created on the reviewer's model and agent, linked from the
    /// original. When the link cannot be written, the new session is deleted,
    /// best effort, and the error returned.
    pub async fn create_or_reuse_review_session(
        &self,
        original: &SessionId,
        directory: &str,
        id: SessionId,
        model: &ModelChoice,
        agent: Option<&str>,
    ) -> Result<ReviewSession, TransportError> {
        let source = self.raw_session(original).await?;
        let source_metadata = metadata_object(source.metadata.as_ref());
        let link = review_link(&source_metadata);
        if let Some(link) = &link {
            if let Ok(existing) = self.session(link).await
                && existing.kind == openchamber_native_core::session::SessionKind::Review
            {
                return Ok(ReviewSession {
                    record: existing,
                    created: false,
                });
            }
            self.patch_metadata(original, without_review_link).await?;
        }
        let title = source
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let request =
            CreateSession::new(id, directory, Some(model.clone()), agent.map(str::to_owned))
                .map_err(session_error)?
                .with_title_and_metadata(
                    format!("Review: {}", title.unwrap_or(original.as_str())),
                    review_marker(original),
                );
        let review = self
            .create_session(&request)
            .await
            .map_err(|error| session_error(error.source))?;
        if let Err(error) = self
            .patch_metadata(original, |metadata| with_review_link(metadata, &review.id))
            .await
        {
            let _ = self.delete_session(&review.id).await;
            return Err(error);
        }
        Ok(ReviewSession {
            record: review,
            created: true,
        })
    }

    /// Puts the reviewer's model on a review session reused from an earlier
    /// review, since the server's prompts switch nothing.
    pub async fn select_session_model(
        &self,
        session: &SessionId,
        model: &ModelChoice,
    ) -> Result<(), TransportError> {
        send_checked(self.model_request(session, model), StatusCode::NO_CONTENT)
            .await
            .map(|_| ())
    }

    /// The same for the reviewer's agent.
    pub async fn select_session_agent(
        &self,
        session: &SessionId,
        agent: &str,
    ) -> Result<(), TransportError> {
        send_checked(self.agent_request(session, agent), StatusCode::NO_CONTENT)
            .await
            .map(|_| ())
    }
}

async fn mutation(response: reqwest::Response) -> Result<AutoReviewMutation, TransportError> {
    let wire: MutationWire = read_json(response).await.map_err(|error| match error {
        TransportError::Malformed(_) => malformed(),
        other => other,
    })?;
    let run = match wire.run {
        Some(wire) => Some(run(wire).ok_or_else(malformed)?),
        None => None,
    };
    Ok(AutoReviewMutation {
        revision: revision(wire.revision).ok_or_else(malformed)?,
        run,
    })
}

fn session_error(error: SessionError) -> TransportError {
    match error {
        SessionError::Transport(error) => error,
        other => TransportError::Malformed(other.to_string()),
    }
}

/// `getReviewSessionID`.
fn review_link(metadata: &Map<String, Value>) -> Option<SessionId> {
    namespace(metadata)
        .get("reviewSessionID")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(SessionId::new)
}

/// `withReviewSessionMarker` on an empty record.
fn review_marker(original: &SessionId) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert(
        "openchamber".to_owned(),
        json!({ "kind": "review", "originalSessionID": original.as_str() }),
    );
    metadata
}

/// `withReviewSessionLink`.
fn with_review_link(metadata: &Map<String, Value>, review: &SessionId) -> Map<String, Value> {
    let mut space = namespace(metadata);
    space.insert("reviewSessionID".to_owned(), json!(review.as_str()));
    with_namespace(metadata, space)
}

/// Drops a stale review link.
fn without_review_link(metadata: &Map<String, Value>) -> Map<String, Value> {
    let mut space = namespace(metadata);
    space.remove("reviewSessionID");
    with_namespace(metadata, space)
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

    fn wire_run() -> Value {
        json!({
            "originalSessionID": "ses_a", "reviewSessionID": "ses_r", "directory": "/repo",
            "status": "running", "step": "handoff", "round": 0, "maxRounds": 5,
            "startedAt": 10, "updatedAt": 11
        })
    }

    #[test]
    fn broadcasts_decode_and_malformed_ones_change_nothing() {
        let update = decode_update(&json!({
            "revision": 4, "originalSessionID": "ses_a", "directory": "/repo", "run": wire_run()
        }))
        .expect("update");
        assert_eq!(update.revision, 4);
        assert_eq!(update.space, None);
        let run = update.run.expect("run");
        assert_eq!(
            (run.status, run.step),
            (RunStatus::Running, RunStep::Handoff)
        );
        let removed = decode_update(&json!({
            "revision": 5, "originalSessionID": "ses_a", "directory": "/repo", "run": null,
            "spaceId": "0123456789ab"
        }))
        .expect("removal");
        assert_eq!(
            (removed.run, removed.space.as_deref()),
            (None, Some("0123456789ab"))
        );
        let mut bad = wire_run();
        bad["status"] = json!("paused");
        assert!(
            decode_update(&json!({
                "revision": 6, "originalSessionID": "ses_a", "directory": "/repo", "run": bad
            }))
            .is_none()
        );
        assert!(decode_update(&json!({ "revision": "x" })).is_none());
    }

    #[test]
    fn the_start_request_matches_the_web_client() {
        let client = ServerClient::new(
            Endpoint::parse("http://127.0.0.1:4799").expect("endpoint"),
            Credential::None,
        )
        .expect("client");
        let start = AutoReviewStart {
            original_session: SessionId::new("ses_a"),
            review_session: SessionId::new("ses_r"),
            directory: "/spaces/0123456789ab/repo".to_owned(),
            handoff: Some(AutoReviewHandoff {
                text: "visible".to_owned(),
                instructions: "rules".to_owned(),
            }),
            prompts: AutoReviewPrompts {
                review_start: "start {{handoff}}".to_owned(),
                review_rules: "rules".to_owned(),
                feedback_to_implementer: "fix {{review_feedback}}".to_owned(),
                response_to_reviewer: "answer {{implementation_response}}".to_owned(),
            },
        };
        let request = client
            .start_auto_review_request(&start)
            .build()
            .expect("request");
        assert_eq!(request.url().path(), "/api/auto-review/runs");
        assert_eq!(
            request
                .headers()
                .get("x-opencode-directory")
                .and_then(|value| value.to_str().ok()),
            Some("%2Fspaces%2F0123456789ab%2Frepo")
        );
        let body: Value = serde_json::from_slice(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("body"),
        )
        .expect("JSON");
        assert_eq!(
            body,
            json!({
                "originalSessionID": "ses_a", "reviewSessionID": "ses_r",
                "directory": "/spaces/0123456789ab/repo",
                "handoff": { "text": "visible", "instructions": "rules" },
                "prompts": {
                    "reviewStart": "start {{handoff}}", "reviewRules": "rules",
                    "feedbackToImplementer": "fix {{review_feedback}}",
                    "responseToReviewer": "answer {{implementation_response}}"
                }
            })
        );
    }

    #[test]
    fn review_links_follow_session_review_metadata() {
        let original = object(json!({ "openchamber": { "pinned": { "at": 1 } } }));
        let linked = with_review_link(&original, &SessionId::new("ses_r"));
        assert_eq!(
            Value::Object(merge_patch(&original, &linked)),
            json!({ "openchamber": { "reviewSessionID": "ses_r" } })
        );
        assert_eq!(review_link(&linked), Some(SessionId::new("ses_r")));
        assert_eq!(without_review_link(&linked), original);
        assert_eq!(
            Value::Object(review_marker(&SessionId::new("ses_a"))),
            json!({ "openchamber": { "kind": "review", "originalSessionID": "ses_a" } })
        );
    }
}
