//! Project knowledge (`/api/project-context/:projectId`), as
//! `packages/ui/src/lib/projectContextApi.ts` calls it: the stored notes,
//! todos and plans, one write per note, todo or plan, plan markdown by id,
//! and the session a plan was sent to.
//!
//! The project id is derived from the owner's path (`ProjectId::from_path`),
//! never a settings id; the server names its storage after it. Plans are
//! addressed by id only, so no path from this side reaches the server's
//! files. Every function errs on failure: a read never answers an empty
//! context a caller could take for "nothing stored". A write that names an
//! entry the server no longer has answers `None`.

use openchamber_native_core::knowledge::{
    Knowledge, Note, NoteSource, Plan, PlanProgress, PlanSource, Todo,
};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Map, Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;

/// A plan's stored markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDocument {
    pub plan: Plan,
    /// The whole file, heading included, as the editor shows it.
    pub raw: String,
    /// Where the file is on the server's machine, only to name it to an agent.
    pub path: Option<String>,
}

/// Why a project knowledge request did not succeed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KnowledgeError {
    /// The server declined, with its explanation when it sent one.
    #[error("the server refused the project knowledge request (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl KnowledgeError {
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            Self::Transport(_) => None,
        }
    }
}

/// How a todo changes, one at a time (`ProjectTodoChange`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TodoChange {
    Create { id: String, text: String },
    Complete { id: String, completed: bool },
    Delete { id: String },
}

fn malformed(what: &str) -> KnowledgeError {
    TransportError::Malformed(format!("unrecognized project knowledge {what}")).into()
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn time(object: &Map<String, Value>, key: &str) -> i64 {
    object
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .map_or(0, |value| value as i64)
}

fn decode_note(value: &Value) -> Option<Note> {
    let object = value.as_object()?;
    Some(Note {
        id: text(object, "id")?,
        body: object.get("body")?.as_str()?.to_owned(),
        created_at: time(object, "createdAt"),
        updated_at: time(object, "updatedAt"),
        source: NoteSource::parse(object.get("source").and_then(Value::as_str).unwrap_or("")),
        plan_id: text(object, "planId"),
    })
}

fn decode_todo(value: &Value) -> Option<Todo> {
    let object = value.as_object()?;
    Some(Todo {
        id: text(object, "id")?,
        text: object.get("text")?.as_str()?.to_owned(),
        completed: object.get("completed").and_then(Value::as_bool) == Some(true),
        created_at: time(object, "createdAt"),
        plan_id: text(object, "planId"),
        depends_on: object
            .get("dependsOn")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().filter(|id| !id.is_empty()).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// `toPlanLink`: a progress or session that does not parse is dropped, so a
/// malformed one shows no badge instead of a wrong one.
fn decode_plan(value: &Value) -> Option<Plan> {
    let object = value.as_object()?;
    let progress = object
        .get("progress")
        .and_then(Value::as_object)
        .and_then(|progress| {
            let count = |key: &str| {
                progress
                    .get(key)?
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
            };
            let (done, total) = (count("done")?, count("total")?);
            (total >= 1 && done <= total).then_some(PlanProgress { done, total })
        });
    let session_id = object
        .get("session")
        .and_then(Value::as_object)
        .filter(|session| session.get("linkedAt").is_some_and(Value::is_number))
        .and_then(|session| text(session, "sessionId"));
    Some(Plan {
        id: text(object, "id")?,
        title: object.get("title")?.as_str()?.to_owned(),
        created_at: time(object, "createdAt"),
        source: match object.get("source").and_then(Value::as_str) {
            Some("shared") => PlanSource::Shared,
            _ => PlanSource::Personal,
        },
        progress,
        session_id,
    })
}

/// `parseContext`: absent lists are empty; a malformed entry is dropped.
fn decode_context(value: &Value) -> Option<Knowledge> {
    let object = value.as_object()?;
    let list = |key: &str| object.get(key).and_then(Value::as_array);
    Some(Knowledge {
        notes: list("notes")
            .map(|notes| notes.iter().filter_map(decode_note).collect())
            .unwrap_or_default(),
        todos: list("todos")
            .map(|todos| todos.iter().filter_map(decode_todo).collect())
            .unwrap_or_default(),
        plans: list("plans")
            .map(|plans| plans.iter().filter_map(decode_plan).collect())
            .unwrap_or_default(),
    })
}

fn todo_json(todo: &Todo) -> Value {
    let mut object = json!({
        "id": todo.id,
        "text": todo.text,
        "completed": todo.completed,
        "createdAt": todo.created_at,
    });
    if let Some(plan) = &todo.plan_id {
        object["planId"] = json!(plan);
    }
    if !todo.depends_on.is_empty() {
        object["dependsOn"] = json!(todo.depends_on);
    }
    object
}

impl ServerClient {
    fn context_url(&self, project_id: &str, rest: &[&str]) -> Url {
        let mut url = self.endpoint().url("api/project-context/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments.pop_if_empty().push(project_id);
            for segment in rest {
                segments.push(segment);
            }
        }
        url
    }

    /// The answer's JSON, `None` for a 404 when `gone_is_none`.
    async fn context_request(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        gone_is_none: bool,
    ) -> Result<Option<Value>, KnowledgeError> {
        let mut request = self.authorized(method, url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TransportError::Redirected.into());
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(TransportError::AuthenticationRequired.into());
        }
        if status == StatusCode::NOT_FOUND && gone_is_none {
            return Ok(None);
        }
        let body = response.json::<Value>().await.ok();
        if !status.is_success() {
            return Err(KnowledgeError::Refused {
                status: status.as_u16(),
                message: body
                    .as_ref()
                    .and_then(|body| body.get("error"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .map(str::to_owned),
            });
        }
        body.map(Some).ok_or_else(|| malformed("answer"))
    }

    async fn context_answer(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        gone_is_none: bool,
    ) -> Result<Option<Knowledge>, KnowledgeError> {
        match self
            .context_request(method, url, body, gone_is_none)
            .await?
        {
            None => Ok(None),
            Some(body) => decode_context(&body)
                .map(Some)
                .ok_or_else(|| malformed("context")),
        }
    }

    /// The owner's notes, todos and plans.
    pub async fn project_knowledge(&self, project_id: &str) -> Result<Knowledge, KnowledgeError> {
        self.context_answer(Method::GET, self.context_url(project_id, &[]), None, false)
            .await?
            .ok_or_else(|| malformed("context"))
    }

    /// One todo; the server applies it to its current list, so a todo an
    /// agent added meanwhile survives. `None` when the todo is already gone.
    pub async fn change_todo(
        &self,
        project_id: &str,
        change: &TodoChange,
    ) -> Result<Option<Knowledge>, KnowledgeError> {
        let (method, url, body) = match change {
            TodoChange::Create { id, text } => (
                Method::POST,
                self.context_url(project_id, &["todos"]),
                Some(json!({ "id": id, "text": text })),
            ),
            TodoChange::Complete { id, completed } => (
                Method::PATCH,
                self.context_url(project_id, &["todos", id]),
                Some(json!({ "completed": completed })),
            ),
            TodoChange::Delete { id } => (
                Method::DELETE,
                self.context_url(project_id, &["todos", id]),
                None,
            ),
        };
        let gone_is_none = !matches!(change, TodoChange::Create { .. });
        self.context_answer(method, url, body, gone_is_none).await
    }

    /// The whole list, for clearing completed todos. Plan links and
    /// dependencies go along, so nothing the panel does not show is lost.
    pub async fn save_todos(
        &self,
        project_id: &str,
        todos: &[Todo],
    ) -> Result<Knowledge, KnowledgeError> {
        let list: Vec<Value> = todos.iter().map(todo_json).collect();
        self.context_answer(
            Method::PUT,
            self.context_url(project_id, &["todos"]),
            Some(json!({ "todos": list })),
            false,
        )
        .await?
        .ok_or_else(|| malformed("context"))
    }

    pub async fn create_note(
        &self,
        project_id: &str,
        body: &str,
    ) -> Result<(Note, Knowledge), KnowledgeError> {
        let answer = self
            .context_request(
                Method::POST,
                self.context_url(project_id, &["notes"]),
                Some(json!({ "body": body })),
                false,
            )
            .await?
            .ok_or_else(|| malformed("note"))?;
        let note = answer
            .get("note")
            .and_then(decode_note)
            .ok_or_else(|| malformed("note"))?;
        let context = answer
            .get("context")
            .and_then(decode_context)
            .ok_or_else(|| malformed("context"))?;
        Ok((note, context))
    }

    /// Saves a note's text; `None` when the note is gone.
    pub async fn update_note(
        &self,
        project_id: &str,
        note_id: &str,
        body: &str,
    ) -> Result<Option<Note>, KnowledgeError> {
        let Some(answer) = self
            .context_request(
                Method::PATCH,
                self.context_url(project_id, &["notes", note_id]),
                Some(json!({ "body": body })),
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        answer
            .get("note")
            .and_then(decode_note)
            .map(Some)
            .ok_or_else(|| malformed("note"))
    }

    pub async fn delete_note(
        &self,
        project_id: &str,
        note_id: &str,
    ) -> Result<Option<Knowledge>, KnowledgeError> {
        self.context_answer(
            Method::DELETE,
            self.context_url(project_id, &["notes", note_id]),
            None,
            true,
        )
        .await
    }

    /// Creates a plan from a title and body; the server picks its id and file.
    pub async fn create_plan(
        &self,
        project_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(Plan, Knowledge), KnowledgeError> {
        let answer = self
            .context_request(
                Method::POST,
                self.context_url(project_id, &["plans"]),
                Some(json!({ "title": title, "body": body })),
                false,
            )
            .await?
            .ok_or_else(|| malformed("plan"))?;
        plan_with_context(&answer)
    }

    /// The plan's markdown; `None` when the plan or its file is gone.
    pub async fn plan_document(
        &self,
        project_id: &str,
        plan_id: &str,
    ) -> Result<Option<PlanDocument>, KnowledgeError> {
        let Some(answer) = self
            .context_request(
                Method::GET,
                self.context_url(project_id, &["plans", plan_id]),
                None,
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        let plan = decode_plan(&answer).ok_or_else(|| malformed("plan"))?;
        let raw = answer
            .get("raw")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("plan"))?
            .to_owned();
        Ok(Some(PlanDocument {
            plan,
            raw,
            path: answer
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }))
    }

    /// Overwrites a plan's markdown with the editor's buffer; `None` when the
    /// plan is gone, so a deleted plan is reported, never recreated.
    pub async fn save_plan(
        &self,
        project_id: &str,
        plan_id: &str,
        raw: &str,
    ) -> Result<Option<(Plan, String)>, KnowledgeError> {
        let Some(answer) = self
            .context_request(
                Method::PUT,
                self.context_url(project_id, &["plans", plan_id]),
                Some(json!({ "raw": raw })),
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        let plan = answer
            .get("plan")
            .and_then(decode_plan)
            .ok_or_else(|| malformed("plan"))?;
        let saved = answer
            .get("raw")
            .and_then(Value::as_str)
            .unwrap_or(raw)
            .to_owned();
        Ok(Some((plan, saved)))
    }

    pub async fn delete_plan(
        &self,
        project_id: &str,
        plan_id: &str,
    ) -> Result<Option<Knowledge>, KnowledgeError> {
        self.context_answer(
            Method::DELETE,
            self.context_url(project_id, &["plans", plan_id]),
            None,
            true,
        )
        .await
    }

    /// Records the session a plan was just sent to.
    pub async fn link_plan_session(
        &self,
        project_id: &str,
        plan_id: &str,
        session_id: &str,
    ) -> Result<Option<(Plan, Knowledge)>, KnowledgeError> {
        let Some(answer) = self
            .context_request(
                Method::PUT,
                self.context_url(project_id, &["plans", plan_id, "session"]),
                Some(json!({ "sessionId": session_id })),
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        plan_with_context(&answer).map(Some)
    }

    /// Moves a plan into the team's shared folder (`share`) or back
    /// (`unshare`); it gets a new id there.
    pub async fn move_plan(
        &self,
        project_id: &str,
        plan_id: &str,
        share: bool,
    ) -> Result<Option<(Plan, Knowledge)>, KnowledgeError> {
        let direction = if share { "share" } else { "unshare" };
        let Some(answer) = self
            .context_request(
                Method::POST,
                self.context_url(project_id, &["plans", plan_id, direction]),
                None,
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        plan_with_context(&answer).map(Some)
    }
}

fn plan_with_context(answer: &Value) -> Result<(Plan, Knowledge), KnowledgeError> {
    let plan = answer
        .get("plan")
        .and_then(decode_plan)
        .ok_or_else(|| malformed("plan"))?;
    let context = answer
        .get("context")
        .and_then(decode_context)
        .ok_or_else(|| malformed("context"))?;
    Ok((plan, context))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_keep_valid_entries_and_drop_broken_badges() {
        let context = decode_context(&json!({
            "notes": [{"id": "n1", "body": "# Title", "createdAt": 5, "updatedAt": 6, "source": "agent"}, {"id": "", "body": "x"}],
            "todos": [{"id": "t1", "text": "Ship", "completed": true, "planId": "p1", "dependsOn": ["t0", ""]}],
            "plans": [
                {"id": "p1", "title": "Launch", "createdAt": 1, "progress": {"done": 3, "total": 2}, "session": {"sessionId": "ses", "linkedAt": 4}},
                {"id": "p2", "title": "Shared", "source": "shared", "progress": {"done": 1, "total": 4}, "session": {"sessionId": "ses"}}
            ],
            "sharedPlansDir": null
        }))
        .expect("context");
        assert_eq!(context.notes.len(), 1);
        assert_eq!(context.notes[0].source, NoteSource::Agent);
        assert_eq!(context.todos[0].depends_on, ["t0"]);
        assert_eq!(context.todos[0].plan_id.as_deref(), Some("p1"));
        assert_eq!(
            context.plans[0].progress, None,
            "done above total is dropped"
        );
        assert_eq!(context.plans[0].session_id.as_deref(), Some("ses"));
        assert_eq!(context.plans[1].source, PlanSource::Shared);
        assert_eq!(
            context.plans[1].progress,
            Some(PlanProgress { done: 1, total: 4 })
        );
        assert_eq!(
            context.plans[1].session_id, None,
            "a session without a time is dropped"
        );
        assert_eq!(decode_context(&json!({})), Some(Knowledge::default()));
        assert!(decode_context(&json!([])).is_none());
    }

    #[test]
    fn whole_list_writes_carry_links_and_dependencies() {
        let todo = Todo {
            id: "t2".into(),
            text: "Build".into(),
            completed: false,
            created_at: 7,
            plan_id: Some("p1".into()),
            depends_on: vec!["t1".into()],
        };
        assert_eq!(
            todo_json(&todo),
            json!({"id": "t2", "text": "Build", "completed": false, "createdAt": 7, "planId": "p1", "dependsOn": ["t1"]})
        );
        let standalone = Todo {
            plan_id: None,
            depends_on: Vec::new(),
            ..todo
        };
        assert!(todo_json(&standalone).get("dependsOn").is_none());
    }
}
