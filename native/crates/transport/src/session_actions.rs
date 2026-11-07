//! Session management from the sidebar menu: rename, delete, archive,
//! restore, pin and work state; and the branching actions of the chat: fork,
//! staged revert, the `/btw` metadata links and stopping a subagent.
//!
//! Rename and delete are OpenCode's `session.update` and `session.remove`
//! (`PATCH` and `DELETE /api/session/:id`). Archive state and metadata are
//! OpenChamber's own routes (`packages/ui/src/sync/session-archive-batch.ts`):
//! OpenCode 2.x accepts metadata only at creation, so the OpenChamber server
//! merge-patches it and folds it into every record it serves. No call here
//! retries; a lost answer leaves its outcome to the next authoritative read
//! or event.

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::session::{MetadataFacts, SessionRecord};
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;
use crate::projection;
use crate::prompt_parts::{Metadata, SyntheticContext};
use crate::wire;

/// The work state the user sets, `metadata.openchamber.work.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    /// Track as in work.
    Open,
    Done,
}

/// What a delete established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deleted {
    Removed,
    /// The server no longer had the session, as after a parent's cascade.
    AlreadyGone,
}

/// The sessions an archive request stamped, and those it could not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Archived {
    /// Each archived session with the stamp the server stored.
    pub archived: Vec<(SessionId, i64)>,
    pub failed: Vec<SessionId>,
}

/// The sessions a restore request brought back, and those it could not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    pub restored: Vec<SessionId>,
    pub failed: Vec<SessionId>,
}

#[derive(Deserialize)]
struct ArchiveStamp {
    id: String,
    #[serde(rename = "archivedAt")]
    archived_at: i64,
}

#[derive(Deserialize)]
struct ArchiveResponse {
    archived: Vec<ArchiveStamp>,
    #[serde(rename = "failedIds")]
    failed_ids: Vec<String>,
}

#[derive(Deserialize)]
struct RestoreStamp {
    id: String,
    #[serde(rename = "archivedAt")]
    archived_at: Option<i64>,
}

#[derive(Deserialize)]
struct RestoreResponse {
    restored: Vec<RestoreStamp>,
    #[serde(rename = "failedIds")]
    failed_ids: Vec<String>,
}

#[derive(Deserialize)]
struct MetadataResponse {
    metadata: Map<String, Value>,
}

impl ServerClient {
    pub(crate) fn session_url(&self, id: &SessionId) -> Url {
        let mut url = self.endpoint().url("api/session/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(id.as_str());
        url
    }

    pub(crate) fn openchamber_sessions_url(&self, segments: &[&str]) -> Url {
        let mut url = self.endpoint().url("api/openchamber/sessions/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .extend(segments);
        url
    }

    pub(crate) fn rename_request(
        &self,
        id: &SessionId,
        title: &str,
    ) -> crate::request::RequestBuilder {
        self.authorized(Method::PATCH, self.session_url(id))
            .json(&json!({ "title": title }))
    }

    pub(crate) fn delete_request(&self, id: &SessionId) -> crate::request::RequestBuilder {
        self.authorized(Method::DELETE, self.session_url(id))
    }

    pub(crate) fn archive_request(
        &self,
        directory: &str,
        ids: &[SessionId],
        archived_at: i64,
    ) -> crate::request::RequestBuilder {
        let ids: Vec<&str> = ids.iter().map(SessionId::as_str).collect();
        self.authorized(Method::POST, self.openchamber_sessions_url(&["archive"]))
            .json(&json!({ "directory": directory, "ids": ids, "archivedAt": archived_at }))
    }

    pub(crate) fn restore_request(&self, ids: &[SessionId]) -> crate::request::RequestBuilder {
        let ids: Vec<&str> = ids.iter().map(SessionId::as_str).collect();
        self.authorized(Method::POST, self.openchamber_sessions_url(&["unarchive"]))
            .json(&json!({ "ids": ids }))
    }

    pub(crate) fn metadata_request(
        &self,
        id: &SessionId,
        patch: &Map<String, Value>,
    ) -> crate::request::RequestBuilder {
        self.authorized(
            Method::POST,
            self.openchamber_sessions_url(&[id.as_str(), "metadata"]),
        )
        .json(&json!({ "patch": patch }))
    }

    /// One session's record as the server serves it, with its metadata.
    pub async fn session(&self, id: &SessionId) -> Result<SessionRecord, TransportError> {
        Ok(projection::session(self.raw_session(id).await?))
    }

    pub(crate) async fn raw_session(
        &self,
        id: &SessionId,
    ) -> Result<wire::Session, TransportError> {
        let response = self
            .authorized(Method::GET, self.session_url(id))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: wire::Single<wire::Session> = read_json(response).await?;
        if reply.data.id != id.as_str() {
            return Err(TransportError::Malformed(
                "the server answered for another session".to_owned(),
            ));
        }
        Ok(reply.data)
    }

    /// Renames a session and returns its record as the server then serves it.
    ///
    /// `session.update` answers with nothing, so the record is read back, as
    /// `updateSessionTitle` does. A name the user gave is noted in the
    /// metadata so the agent's `work.name` keeps it; that note failing does
    /// not fail the rename.
    pub async fn rename_session(
        &self,
        id: &SessionId,
        title: &str,
    ) -> Result<SessionRecord, TransportError> {
        expect_status(
            self.rename_request(id, title).send().await,
            StatusCode::NO_CONTENT,
        )?;
        let session = self.raw_session(id).await?;
        let current = metadata_object(session.metadata.as_ref());
        let _ = self
            .update_metadata(id, &current, &with_user_work_title(&current, title))
            .await;
        Ok(projection::session(session))
    }

    /// Whether the user named the session (`carriedWorkTitleOwner`), so a
    /// worktree made for it keeps the name over the agent's.
    pub async fn session_titled_by_user(&self, id: &SessionId) -> Result<bool, TransportError> {
        let session = self.raw_session(id).await?;
        let by = namespace(&metadata_object(session.metadata.as_ref()))
            .get("workTitle")
            .and_then(Value::as_object)
            .filter(|title| {
                title
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            })
            .and_then(|title| title.get("by"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(by.as_deref() == Some("user"))
    }

    /// Deletes one session. A 404 means the session is already gone, as when
    /// the server cascaded its parent's deletion.
    pub async fn delete_session(&self, id: &SessionId) -> Result<Deleted, TransportError> {
        match expect_status(self.delete_request(id).send().await, StatusCode::NO_CONTENT) {
            Ok(()) => Ok(Deleted::Removed),
            Err(TransportError::Http { status: 404 }) => Ok(Deleted::AlreadyGone),
            Err(error) => Err(error),
        }
    }

    /// Archives sessions of one directory in one request.
    pub async fn archive_sessions(
        &self,
        directory: &str,
        ids: &[SessionId],
        archived_at: i64,
    ) -> Result<Archived, TransportError> {
        let response = self
            .archive_request(directory, ids, archived_at)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: ArchiveResponse = read_json(response).await.map_err(receipt_error)?;
        Ok(Archived {
            archived: reply
                .archived
                .into_iter()
                .filter(|stamp| !stamp.id.is_empty())
                .map(|stamp| (SessionId::new(stamp.id), stamp.archived_at))
                .collect(),
            failed: reply
                .failed_ids
                .into_iter()
                .filter(|id| !id.is_empty())
                .map(SessionId::new)
                .collect(),
        })
    }

    /// Clears the archive stamp of sessions.
    pub async fn restore_sessions(&self, ids: &[SessionId]) -> Result<Restored, TransportError> {
        let response = self
            .restore_request(ids)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: RestoreResponse = read_json(response).await.map_err(receipt_error)?;
        if reply
            .restored
            .iter()
            .any(|stamp| stamp.archived_at.is_some())
        {
            return Err(TransportError::Malformed(
                "a restored session is still archived".to_owned(),
            ));
        }
        Ok(Restored {
            restored: reply
                .restored
                .into_iter()
                .filter(|stamp| !stamp.id.is_empty())
                .map(|stamp| SessionId::new(stamp.id))
                .collect(),
            failed: reply
                .failed_ids
                .into_iter()
                .filter(|id| !id.is_empty())
                .map(SessionId::new)
                .collect(),
        })
    }

    /// Pins or unpins a session for every client of the server.
    pub async fn set_session_pinned(
        &self,
        id: &SessionId,
        pinned: bool,
        now: i64,
    ) -> Result<MetadataFacts, TransportError> {
        self.patch_metadata(id, |metadata| with_pinned(metadata, pinned, now))
            .await
    }

    /// Tracks a session as in work or marks its work done.
    pub async fn set_session_work_state(
        &self,
        id: &SessionId,
        state: WorkState,
        now: i64,
    ) -> Result<MetadataFacts, TransportError> {
        self.patch_metadata(id, |metadata| with_work_state(metadata, state, now))
            .await
    }

    pub(crate) fn fork_request(
        &self,
        id: &SessionId,
        before: Option<&MessageId>,
    ) -> crate::request::RequestBuilder {
        let mut url = self.session_url(id);
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .push("fork");
        // The pinned client sends `{ before }`; JSON drops an absent cut.
        let body = match before {
            Some(before) => json!({ "before": before.as_str() }),
            None => json!({}),
        };
        self.authorized(Method::POST, url).json(&body)
    }

    fn revert_url(&self, id: &SessionId, stage: bool) -> Url {
        let mut url = self.session_url(id);
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments.push("revert");
            if stage {
                segments.push("stage");
            }
        }
        url
    }

    pub(crate) fn stage_revert_request(
        &self,
        id: &SessionId,
        message: &MessageId,
    ) -> crate::request::RequestBuilder {
        self.authorized(Method::POST, self.revert_url(id, true))
            .json(&json!({ "messageID": message.as_str() }))
    }

    pub(crate) fn clear_revert_request(&self, id: &SessionId) -> crate::request::RequestBuilder {
        self.authorized(Method::DELETE, self.revert_url(id, false))
    }

    /// Forks a session (`session.fork`): the new session holds the messages
    /// before `before`, or all of them without it. Nothing is retried; a lost
    /// answer may still have created the fork, which `session.forked` reports.
    pub async fn fork_session(
        &self,
        id: &SessionId,
        before: Option<&MessageId>,
    ) -> Result<SessionRecord, TransportError> {
        let response = self
            .fork_request(id, before)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: wire::Single<wire::Session> =
            read_json(response).await.map_err(receipt_error)?;
        if reply.data.id.is_empty() || reply.data.id == id.as_str() {
            return Err(TransportError::Malformed(
                "the fork answer names no new session".to_owned(),
            ));
        }
        Ok(projection::session(reply.data))
    }

    /// Drops what a user fork must not keep of its source's copied metadata
    /// (`withoutSourceOwnedLinks`): the source's `/btw` and review links, a
    /// `/btw` or scheduled marker, and an active goal, which arrives paused.
    /// Returns `None` when there was nothing to change.
    pub async fn clean_fork_links(
        &self,
        fork: &SessionId,
    ) -> Result<Option<MetadataFacts>, TransportError> {
        let session = self.raw_session(fork).await?;
        let current = metadata_object(session.metadata.as_ref());
        let next = without_source_owned_links(&current);
        if next == current {
            return Ok(None);
        }
        let metadata = self.update_metadata(fork, &current, &next).await?;
        Ok(Some(projection::metadata_facts(Some(&Value::Object(
            metadata,
        )))))
    }

    /// Stages a revert to before `message` (`session.revert.stage`): later
    /// messages hide and their file changes roll back until the next prompt
    /// commits it or [`Self::clear_revert`] puts everything back. Returns the
    /// marker the server stored.
    pub async fn stage_revert(
        &self,
        id: &SessionId,
        message: &MessageId,
    ) -> Result<MessageId, TransportError> {
        let response = self
            .stage_revert_request(id, message)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: wire::Single<wire::Revert> = read_json(response).await.map_err(receipt_error)?;
        if reply.data.message_id.is_empty() {
            return Err(TransportError::Malformed(
                "the revert answer has no marker".to_owned(),
            ));
        }
        Ok(MessageId::new(reply.data.message_id))
    }

    /// Clears a staged revert (`session.revert.clear`).
    pub async fn clear_revert(&self, id: &SessionId) -> Result<(), TransportError> {
        expect_status(
            self.clear_revert_request(id).send().await,
            StatusCode::NO_CONTENT,
        )
    }

    /// Marks a fresh fork as `parent`'s `/btw` side conversation
    /// (`withBtwSessionMarker`). The fork copied the parent's metadata, so its
    /// `openchamber` namespace is replaced, not merged.
    pub async fn mark_btw_fork(
        &self,
        fork: &SessionId,
        parent: &SessionId,
        boundary: Option<&MessageId>,
    ) -> Result<MetadataFacts, TransportError> {
        self.patch_metadata(fork, |metadata| {
            let mut space = Map::new();
            space.insert("kind".to_owned(), json!("btw"));
            space.insert("originalSessionID".to_owned(), json!(parent.as_str()));
            if let Some(boundary) = boundary {
                space.insert("btwBoundaryMessageID".to_owned(), json!(boundary.as_str()));
            }
            with_namespace(metadata, space)
        })
        .await
    }

    /// Links `parent` to its active side conversation (`withBtwSessionLink`).
    pub async fn link_btw(
        &self,
        parent: &SessionId,
        fork: &SessionId,
    ) -> Result<MetadataFacts, TransportError> {
        self.patch_metadata(parent, |metadata| {
            let mut space = namespace(metadata);
            space.insert("btwSessionID".to_owned(), json!(fork.as_str()));
            with_namespace(metadata, space)
        })
        .await
    }

    /// Unlinks `parent`, but only while it still points at `fork`
    /// (`withoutBtwSessionLink`).
    pub async fn unlink_btw(
        &self,
        parent: &SessionId,
        fork: &SessionId,
    ) -> Result<MetadataFacts, TransportError> {
        self.patch_metadata(parent, |metadata| without_btw_link(metadata, fork))
            .await
    }

    /// Keeps a side conversation as an ordinary session
    /// (`promoteBtwSession`): drops the fork's marker for `btwPromoted`, then
    /// unlinks the parent. When the unlink fails, the fork's metadata is put
    /// back, best effort, and the unlink's error is returned.
    pub async fn promote_btw(
        &self,
        parent: &SessionId,
        fork: &SessionId,
    ) -> Result<(), TransportError> {
        let session = self.raw_session(fork).await?;
        let original = metadata_object(session.metadata.as_ref());
        let promoted = without_btw_marker(&original);
        let stored = self.update_metadata(fork, &original, &promoted).await?;
        if let Err(error) = self.unlink_btw(parent, fork).await {
            let _ = self.update_metadata(fork, &stored, &original).await;
            return Err(error);
        }
        Ok(())
    }

    /// Stops a subagent (`stopSubagent`): first a note in the parent that the
    /// stop was the user's, so the agent does not start it again, then the
    /// child's interrupt. Nothing is stopped when the note was not admitted.
    pub async fn stop_subagent(
        &self,
        parent: &SessionId,
        child: &SessionId,
        note_id: &MessageId,
        description: Option<&str>,
    ) -> Result<(), TransportError> {
        let note = subagent_cancellation_note(note_id.clone(), child, description);
        let response = self
            .synthetic_request(parent, &note, None)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TransportError::Redirected);
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(TransportError::AuthenticationRequired);
        }
        if status != StatusCode::OK {
            return Err(TransportError::Http {
                status: status.as_u16(),
            });
        }
        self.abort_session(child)
            .await
            .map(|_| ())
            .map_err(|error| error.source)
    }

    /// `patchSessionMetadata`: reads the record, applies `update` and sends
    /// the difference as a merge patch, so keys other features own stay.
    pub(crate) async fn patch_metadata(
        &self,
        id: &SessionId,
        update: impl FnOnce(&Map<String, Value>) -> Map<String, Value>,
    ) -> Result<MetadataFacts, TransportError> {
        let session = self.raw_session(id).await?;
        let current = metadata_object(session.metadata.as_ref());
        let next = update(&current);
        let metadata = self.update_metadata(id, &current, &next).await?;
        Ok(projection::metadata_facts(Some(&Value::Object(metadata))))
    }

    pub(crate) async fn update_metadata(
        &self,
        id: &SessionId,
        current: &Map<String, Value>,
        next: &Map<String, Value>,
    ) -> Result<Map<String, Value>, TransportError> {
        let patch = merge_patch(current, next);
        if patch.is_empty() {
            return Ok(next.clone());
        }
        let response = self
            .metadata_request(id, &patch)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let reply: MetadataResponse = read_json(response).await.map_err(receipt_error)?;
        Ok(reply.metadata)
    }
}

pub(crate) fn expect_status(
    sent: Result<reqwest::Response, reqwest::Error>,
    expected: StatusCode,
) -> Result<(), TransportError> {
    let status = sent.map_err(TransportError::from_request)?.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(TransportError::AuthenticationRequired);
    }
    if status != expected {
        return Err(TransportError::Http {
            status: status.as_u16(),
        });
    }
    Ok(())
}

pub(crate) fn receipt_error(error: TransportError) -> TransportError {
    match error {
        // Serde errors can quote the payload. Keep it out of diagnostics.
        TransportError::Malformed(_) => {
            TransportError::Malformed("unrecognized session action receipt".to_owned())
        }
        other => other,
    }
}

pub(crate) fn metadata_object(metadata: Option<&Value>) -> Map<String, Value> {
    metadata
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The `openchamber` namespace of a metadata record, empty when absent or
/// not an object.
pub(crate) fn namespace(metadata: &Map<String, Value>) -> Map<String, Value> {
    metadata
        .get("openchamber")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn with_namespace(
    metadata: &Map<String, Value>,
    namespace: Map<String, Value>,
) -> Map<String, Value> {
    let mut next = metadata.clone();
    next.insert("openchamber".to_owned(), Value::Object(namespace));
    next
}

/// `withSessionPinned`: pinning keeps an existing pin's time.
fn with_pinned(metadata: &Map<String, Value>, pinned: bool, now: i64) -> Map<String, Value> {
    let mut space = namespace(metadata);
    let is_pinned = space
        .get("pinned")
        .and_then(|pin| pin.get("at"))
        .is_some_and(Value::is_number);
    if is_pinned == pinned {
        return metadata.clone();
    }
    if pinned {
        space.insert("pinned".to_owned(), json!({ "at": now }));
    } else {
        space.remove("pinned");
    }
    with_namespace(metadata, space)
}

/// `withSessionWorkState`: Track opens the work as the user's; Done keeps
/// when and by whom it was opened and drops Jev's hints.
fn with_work_state(
    metadata: &Map<String, Value>,
    state: WorkState,
    now: i64,
) -> Map<String, Value> {
    let mut space = namespace(metadata);
    let current = space.get("work").and_then(Value::as_object);
    let current_state = current
        .and_then(|work| work.get("state"))
        .and_then(Value::as_str);
    let target = match state {
        WorkState::Open => "open",
        WorkState::Done => "done",
    };
    if current_state == Some(target) && matches!(current_state, Some("open" | "done")) {
        return metadata.clone();
    }
    let work = match state {
        WorkState::Open => json!({ "state": "open", "openedAt": now, "openedBy": "user" }),
        WorkState::Done => {
            let mut work = Map::new();
            work.insert("state".to_owned(), json!("done"));
            work.insert("doneAt".to_owned(), json!(now));
            for key in ["openedAt", "openedBy"] {
                if let Some(value) = current.and_then(|work| work.get(key)) {
                    work.insert(key.to_owned(), value.clone());
                }
            }
            Value::Object(work)
        }
    };
    space.insert("work".to_owned(), work);
    with_namespace(metadata, space)
}

/// `withoutSourceOwnedLinks`.
fn without_source_owned_links(metadata: &Map<String, Value>) -> Map<String, Value> {
    let Some(space) = metadata.get("openchamber").and_then(Value::as_object) else {
        return metadata.clone();
    };
    let mut next = space.clone();
    next.remove("btwSessionID");
    next.remove("reviewSessionID");
    if next.get("kind").and_then(Value::as_str) == Some("btw") {
        for key in ["kind", "originalSessionID", "btwBoundaryMessageID"] {
            next.remove(key);
        }
    }
    if next.get("kind").and_then(Value::as_str) == Some("scheduled") {
        next.remove("kind");
    }
    if let Some(goal) = next.get_mut("goal").and_then(Value::as_object_mut)
        && goal.get("status").and_then(Value::as_str) == Some("active")
    {
        goal.insert("status".to_owned(), json!("paused"));
        goal.insert("statusReason".to_owned(), json!("paused in fork"));
    }
    if &next == space {
        return metadata.clone();
    }
    let mut result = metadata.clone();
    if next.is_empty() {
        result.remove("openchamber");
    } else {
        result.insert("openchamber".to_owned(), Value::Object(next));
    }
    result
}

/// `withoutBtwSessionLink`: the namespace goes when nothing else is in it.
fn without_btw_link(metadata: &Map<String, Value>, fork: &SessionId) -> Map<String, Value> {
    let mut space = namespace(metadata);
    if space.get("btwSessionID").and_then(Value::as_str) != Some(fork.as_str()) {
        return metadata.clone();
    }
    space.remove("btwSessionID");
    if space.is_empty() {
        let mut next = metadata.clone();
        next.remove("openchamber");
        return next;
    }
    with_namespace(metadata, space)
}

/// `withoutBtwSessionMarker`: `btwPromoted` stays behind, because the btw
/// instructions stay in the transcript.
fn without_btw_marker(metadata: &Map<String, Value>) -> Map<String, Value> {
    let mut space = namespace(metadata);
    if space.get("kind").and_then(Value::as_str) != Some("btw") {
        return metadata.clone();
    }
    for key in ["kind", "originalSessionID", "btwBoundaryMessageID"] {
        space.remove(key);
    }
    space.insert("btwPromoted".to_owned(), json!(true));
    with_namespace(metadata, space)
}

/// `subagentCancellationNote`: model-facing text the UI never shows.
fn subagent_cancellation_note(
    id: MessageId,
    child: &SessionId,
    description: Option<&str>,
) -> SyntheticContext {
    let named = match description {
        Some(description) => format!("the subagent \"{description}\""),
        None => "a subagent".to_owned(),
    };
    let mut metadata = Map::new();
    metadata.insert(
        "openchamberSubagentCancellation".to_owned(),
        json!({ "sessionID": child.as_str() }),
    );
    SyntheticContext {
        id,
        text: [
            format!("The user stopped {named} (sessionID: {child})."),
            "The result for it that follows reports that the subagent was cancelled.".to_owned(),
            "That is how the stop shows up: the subagent did not fail on its own, the user cancelled it.".to_owned(),
            "Do not start it again just because of that result. The same work is fine to delegate later when the task or the user needs it.".to_owned(),
        ]
        .join(" "),
        description: Some("Subagent stopped by the user".to_owned()),
        metadata: Some(Metadata(metadata)),
    }
}

/// `withUserWorkTitle`.
fn with_user_work_title(metadata: &Map<String, Value>, title: &str) -> Map<String, Value> {
    let mut space = namespace(metadata);
    space.insert(
        "workTitle".to_owned(),
        json!({ "title": title, "by": "user" }),
    );
    with_namespace(metadata, space)
}

/// `buildMetadataMergePatch`: the RFC 7386 merge patch that turns `current`
/// into `next`. Changed keys carry their value, removed keys `null`, and
/// nested records recurse so sibling keys stay untouched.
pub(crate) fn merge_patch(
    current: &Map<String, Value>,
    next: &Map<String, Value>,
) -> Map<String, Value> {
    let mut patch = Map::new();
    for key in current.keys() {
        if !next.contains_key(key) {
            patch.insert(key.clone(), Value::Null);
        }
    }
    for (key, value) in next {
        let previous = current.get(key);
        if let (Value::Object(value), Some(Value::Object(previous))) = (value, previous) {
            let nested = merge_patch(previous, value);
            if !nested.is_empty() {
                patch.insert(key.clone(), Value::Object(nested));
            }
            continue;
        }
        if previous != Some(value) {
            patch.insert(key.clone(), value.clone());
        }
    }
    patch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::Credential;
    use crate::endpoint::Endpoint;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn client() -> ServerClient {
        ServerClient::new(
            Endpoint::parse("http://127.0.0.1:4799").expect("endpoint"),
            Credential::None,
        )
        .expect("client")
    }

    fn parts(
        builder: crate::request::RequestBuilder,
    ) -> (String, String, Option<Value>, Option<String>) {
        let request = builder.build().expect("request");
        let body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(|bytes| serde_json::from_slice(bytes).expect("JSON body"));
        let content_type = request
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("header").to_owned());
        (
            request.method().to_string(),
            request.url().path().to_owned(),
            body,
            content_type,
        )
    }

    #[test]
    fn opencode_requests_match_the_pinned_client() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/transport.json"))
            .expect("generated fixture");
        let session = SessionId::new("ses_e9b49c443ffefXcWWPhCOga6nH");
        let client = client();
        for (name, builder) in [
            (
                "rename session",
                client.rename_request(&session, "Renamed ✓"),
            ),
            ("delete session", client.delete_request(&session)),
        ] {
            let expected = fixture["requests"]
                .as_array()
                .expect("requests")
                .iter()
                .find(|entry| entry["name"] == name)
                .expect("recorded operation");
            let (method, path, body, content_type) = parts(builder);
            assert_eq!(method, expected["method"], "{name}");
            assert_eq!(path, expected["path"], "{name}");
            assert_eq!(
                content_type.as_deref(),
                expected["contentType"].as_str(),
                "{name}"
            );
            let expected_body: Option<Value> = expected["body"]
                .as_str()
                .map(|body| serde_json::from_str(body).expect("JSON fixture body"));
            assert_eq!(body, expected_body, "{name}");
        }
    }

    #[test]
    fn openchamber_requests_match_the_web_client() {
        let client = client();
        let ids = [SessionId::new("ses_a"), SessionId::new("ses_b")];
        let (method, path, body, _) = parts(client.archive_request("/work/my app", &ids, 42));
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/api/openchamber/sessions/archive")
        );
        assert_eq!(
            body,
            Some(
                json!({ "directory": "/work/my app", "ids": ["ses_a", "ses_b"], "archivedAt": 42 })
            )
        );
        let (_, path, body, _) = parts(client.restore_request(&ids[..1]));
        assert_eq!(path, "/api/openchamber/sessions/unarchive");
        assert_eq!(body, Some(json!({ "ids": ["ses_a"] })));
        let patch = object(json!({ "openchamber": { "pinned": null } }));
        let (_, path, body, _) = parts(client.metadata_request(&ids[0], &patch));
        assert_eq!(path, "/api/openchamber/sessions/ses_a/metadata");
        assert_eq!(
            body,
            Some(json!({ "patch": { "openchamber": { "pinned": null } } }))
        );
    }

    #[test]
    fn pins_patch_only_their_own_key() {
        let current = object(json!({ "openchamber": { "work": { "state": "open" } }, "other": 1 }));
        let pinned = with_pinned(&current, true, 7);
        assert_eq!(
            Value::Object(merge_patch(&current, &pinned)),
            json!({ "openchamber": { "pinned": { "at": 7 } } })
        );
        let unpinned = with_pinned(&pinned, false, 9);
        assert_eq!(
            Value::Object(merge_patch(&pinned, &unpinned)),
            json!({ "openchamber": { "pinned": null } })
        );
        assert_eq!(
            with_pinned(&pinned, true, 9),
            pinned,
            "a pin keeps its time"
        );
    }

    #[test]
    fn work_changes_follow_the_reference_shape() {
        let current = object(json!({ "openchamber": { "work": {
            "state": "open", "openedAt": 3, "openedBy": "auto", "suggestDoneAt": 5
        } } }));
        let done = with_work_state(&current, WorkState::Done, 9);
        assert_eq!(
            Value::Object(merge_patch(&current, &done)),
            json!({ "openchamber": { "work": { "state": "done", "doneAt": 9, "suggestDoneAt": null } } })
        );
        let reopened = with_work_state(&done, WorkState::Open, 11);
        assert_eq!(
            Value::Object(merge_patch(&done, &reopened)),
            json!({ "openchamber": { "work": {
                "state": "open", "openedAt": 11, "openedBy": "user", "doneAt": null
            } } })
        );
        assert!(
            merge_patch(&reopened, &with_work_state(&reopened, WorkState::Open, 12)).is_empty()
        );
        let empty = Map::new();
        assert_eq!(
            Value::Object(merge_patch(
                &empty,
                &with_work_state(&empty, WorkState::Open, 1)
            )),
            json!({ "openchamber": { "work": { "state": "open", "openedAt": 1, "openedBy": "user" } } })
        );
    }

    #[test]
    fn branching_requests_match_the_pinned_client() {
        // `@opencode/client` 2.x: session.fork, session.revert.stage/clear.
        let client = client();
        let session = SessionId::new("ses_a");
        let (method, path, body, _) =
            parts(client.fork_request(&session, Some(&MessageId::new("msg_cut"))));
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/api/session/ses_a/fork")
        );
        assert_eq!(body, Some(json!({ "before": "msg_cut" })));
        let (_, _, body, _) = parts(client.fork_request(&session, None));
        assert_eq!(body, Some(json!({})), "an absent cut copies everything");
        let (method, path, body, _) =
            parts(client.stage_revert_request(&session, &MessageId::new("msg_2")));
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/api/session/ses_a/revert/stage")
        );
        assert_eq!(body, Some(json!({ "messageID": "msg_2" })));
        let (method, path, body, _) = parts(client.clear_revert_request(&session));
        assert_eq!(
            (method.as_str(), path.as_str(), body),
            ("DELETE", "/api/session/ses_a/revert", None)
        );
    }

    #[test]
    fn btw_links_follow_session_btw_metadata() {
        let fork = SessionId::new("ses_fork");
        let parent = object(json!({ "openchamber": { "btwSessionID": "ses_fork" }, "x": 1 }));
        assert_eq!(
            Value::Object(without_btw_link(&parent, &fork)),
            json!({ "x": 1 }),
            "an emptied namespace goes"
        );
        let pinned =
            object(json!({ "openchamber": { "btwSessionID": "ses_fork", "pinned": { "at": 1 } } }));
        assert_eq!(
            Value::Object(merge_patch(&pinned, &without_btw_link(&pinned, &fork))),
            json!({ "openchamber": { "btwSessionID": null } })
        );
        let other = object(json!({ "openchamber": { "btwSessionID": "ses_other" } }));
        assert_eq!(
            without_btw_link(&other, &fork),
            other,
            "a parent linked to another fork stays"
        );
        let marked = object(json!({ "openchamber": {
            "kind": "btw", "originalSessionID": "ses_p", "btwBoundaryMessageID": "msg_b", "pinned": { "at": 1 }
        } }));
        assert_eq!(
            Value::Object(without_btw_marker(&marked)),
            json!({ "openchamber": { "pinned": { "at": 1 }, "btwPromoted": true } })
        );
        let plain = object(json!({ "openchamber": { "kind": "review" } }));
        assert_eq!(without_btw_marker(&plain), plain);
    }

    #[test]
    fn a_fork_drops_source_owned_links_and_pauses_the_goal() {
        let copied = object(json!({ "openchamber": {
            "btwSessionID": "ses_b", "reviewSessionID": "ses_r", "kind": "btw",
            "originalSessionID": "ses_p", "pinned": { "at": 1 },
            "goal": { "id": "g", "status": "active" }
        }, "other": true }));
        assert_eq!(
            Value::Object(without_source_owned_links(&copied)),
            json!({ "openchamber": {
                "pinned": { "at": 1 },
                "goal": { "id": "g", "status": "paused", "statusReason": "paused in fork" }
            }, "other": true })
        );
        let plain = object(json!({ "openchamber": { "pinned": { "at": 1 } } }));
        assert_eq!(without_source_owned_links(&plain), plain);
        let only_link = object(json!({ "openchamber": { "btwSessionID": "ses_b" } }));
        assert_eq!(
            Value::Object(without_source_owned_links(&only_link)),
            json!({})
        );
    }

    #[test]
    fn the_cancellation_note_names_the_child_and_does_not_resume() {
        let note = subagent_cancellation_note(
            MessageId::new("msg_note"),
            &SessionId::new("ses_child"),
            Some("Explore"),
        );
        assert!(
            note.text
                .starts_with("The user stopped the subagent \"Explore\" (sessionID: ses_child).")
        );
        assert_eq!(
            note.metadata.expect("metadata").0,
            object(json!({ "openchamberSubagentCancellation": { "sessionID": "ses_child" } }))
        );
        let (method, path, body, _) = parts(client().synthetic_request(
            &SessionId::new("ses_parent"),
            &subagent_cancellation_note(MessageId::new("msg_note"), &SessionId::new("c"), None),
            None,
        ));
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/api/session/ses_parent/synthetic")
        );
        let body = body.expect("body");
        assert_eq!(body["resume"], json!(false));
        assert_eq!(body["description"], json!("Subagent stopped by the user"));
        assert!(
            body["text"]
                .as_str()
                .expect("text")
                .starts_with("The user stopped a subagent")
        );
    }

    #[test]
    fn a_user_title_note_keeps_the_rest_of_the_namespace() {
        let current = object(json!({ "openchamber": { "pinned": { "at": 1 } } }));
        assert_eq!(
            Value::Object(merge_patch(
                &current,
                &with_user_work_title(&current, "Mine")
            )),
            json!({ "openchamber": { "workTitle": { "title": "Mine", "by": "user" } } })
        );
    }
}
