//! The row menu's AI rename: names a session from its latest completed
//! turns on the server's small model.
//!
//! Follows `useSessionAiRenameAction`, `useSessionAiRename` and
//! `generateAndSaveSessionTitle`. While the menu is open the item checks
//! that the small model can run and that the session has a completed turn,
//! and stays disabled with the reason until both answer. A rename reads the
//! history until three turns or its start, generates, then reads the
//! session again: a title, directory, archive or revert change made
//! meanwhile wins and nothing is saved. The save is a user rename, so the
//! agent's `work.name` keeps it.

use std::collections::HashSet;

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session::{SessionPatch, SessionRecord};
use openchamber_native_core::session_actions::AiRenameState;
use openchamber_native_core::session_markdown::{self, TITLE_SYSTEM_PROMPT};
use openchamber_native_core::transcript::Message;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::small_model::Generation;

use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;

/// Messages per history page while looking for turns.
const PAGE: usize = 100;

/// The open menu's check and the renames that run.
#[derive(Default)]
pub(crate) struct AiRename {
    check: Option<Check>,
    pending: HashSet<SessionKey>,
}

impl AiRename {
    pub(crate) fn switch_runtime(&mut self) {
        self.check = None;
    }
}

/// What the item knows for the session whose menu is open.
struct Check {
    key: SessionKey,
    /// `None` until the history answered.
    turns: Option<Result<bool, ()>>,
    small_model: Option<bool>,
    _task: Task<()>,
}

/// Why a rename stopped.
#[derive(Debug)]
enum RenameError {
    Transport,
    NoTurns,
    Moved,
}

impl From<TransportError> for RenameError {
    fn from(_: TransportError) -> Self {
        Self::Transport
    }
}

/// The session and its newest messages up to three completed turns.
async fn prepare(
    client: &ServerClient,
    session: &SessionId,
    directory: &str,
) -> Result<(SessionRecord, Vec<Message>), RenameError> {
    let record = client.session(session).await?;
    if record.resolved_directory().as_deref() != Some(directory) {
        return Err(RenameError::Moved);
    }
    let page = client.latest_messages(session, PAGE).await?;
    let mut messages = page.messages;
    let mut cursor = page.older;
    let mut visited = HashSet::new();
    loop {
        let turns = session_markdown::title_turns(&messages, record.revert.as_ref()).len();
        let Some(older) = cursor.take().filter(|_| turns < 3) else {
            break;
        };
        if !visited.insert(older.clone()) {
            // Pagination made no progress.
            return Err(RenameError::Transport);
        }
        let page = client.earlier_messages(session, &older, PAGE).await?;
        let mut earlier = page.messages;
        earlier.append(&mut messages);
        messages = earlier;
        cursor = page.older;
    }
    Ok((record, messages))
}

/// Generates and saves the title; `Ok(None)` when the session changed
/// meanwhile and nothing was saved.
async fn generate_and_save(
    client: ServerClient,
    session: SessionId,
    directory: String,
) -> Result<Option<SessionRecord>, RenameError> {
    let (record, messages) = prepare(&client, &session, &directory).await?;
    let turns = session_markdown::title_turns(&messages, record.revert.as_ref());
    let Some(last) = turns.last() else {
        return Err(RenameError::NoTurns);
    };
    let model = last
        .assistant
        .info
        .assistant
        .as_ref()
        .and_then(|facts| facts.model.as_ref());
    let prompt = session_markdown::title_prompt(&record.title, &turns);
    let system = TITLE_SYSTEM_PROMPT.join("\n");
    let text = client
        .generate_with_small_model(&Generation {
            prompt: &prompt,
            system: &system,
            directory: &directory,
            preferred_provider: model.map(|model| model.provider_id.as_str()),
            preferred_model: model.map(|model| model.model_id.as_str()),
        })
        .await?;
    let title = session_markdown::clean_generated_title(&text).ok_or(RenameError::Transport)?;
    let current = client.session(&session).await?;
    let unchanged = current.title == record.title
        && current.directory == record.directory
        && current.time.archived == record.time.archived
        && current.revert == record.revert;
    if !unchanged {
        return Ok(None);
    }
    Ok(Some(client.rename_session(&session, &title).await?))
}

/// Whether the session has a completed turn, from as few pages as needed.
async fn has_turns(
    client: &ServerClient,
    session: &SessionId,
    directory: &str,
) -> Result<bool, RenameError> {
    let (record, messages) = prepare(client, session, directory).await?;
    Ok(!session_markdown::title_turns(&messages, record.revert.as_ref()).is_empty())
}

impl Workspace {
    /// What the AI rename item shows for `session` now.
    pub(crate) fn ai_rename_state(&self, session: &SessionId) -> AiRenameState {
        let key = self.mutation_key(session);
        let rename = &self.session_management.ai_rename;
        if rename.pending.contains(&key) {
            return AiRenameState::Pending;
        }
        let Some(check) = rename.check.as_ref().filter(|check| check.key == key) else {
            return AiRenameState::Checking;
        };
        if check.small_model == Some(false) {
            return AiRenameState::NoSmallModel;
        }
        match check.turns {
            None => AiRenameState::Checking,
            Some(Ok(true)) => AiRenameState::Ready,
            Some(Ok(false)) => AiRenameState::Empty,
            Some(Err(())) => AiRenameState::Unknown,
        }
    }

    /// Starts the checks the open menu's AI rename item waits for.
    pub(crate) fn check_ai_rename(&mut self, session: &SessionId, cx: &mut Context<Self>) {
        let key = self.mutation_key(session);
        if self.session_management.ai_rename.pending.contains(&key) {
            return;
        }
        let Some(directory) = self
            .sessions
            .get(session)
            .and_then(|record| record.resolved_directory())
        else {
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let id = session.clone();
        let read = connection.network.run(async move {
            let small_model = client
                .small_model_available(Some(&directory))
                .await
                .ok()
                .flatten();
            let turns = has_turns(&client, &id, &directory).await.map_err(|_| ());
            (small_model, turns)
        });
        let checked = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                let Some(check) = this
                    .session_management
                    .ai_rename
                    .check
                    .as_mut()
                    .filter(|check| check.key == checked)
                else {
                    return;
                };
                let (small_model, turns) = result.unwrap_or((None, Err(())));
                check.small_model = small_model;
                check.turns = Some(turns);
                this.refresh_session_menu(cx);
                // The header's menu reads the check at each draw.
                cx.notify();
            });
        });
        self.session_management.ai_rename.check = Some(Check {
            key,
            turns: None,
            small_model: None,
            _task: task,
        });
    }

    /// The menu closed: its check stops.
    pub(crate) fn stop_ai_rename_check(&mut self) {
        self.session_management.ai_rename.check = None;
    }

    /// AI rename from the row menu.
    pub(crate) fn ai_rename_session(
        &mut self,
        session: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(session);
        if self.session_management.ai_rename.pending.contains(&key) {
            return;
        }
        let Some(directory) = self
            .sessions
            .get(session)
            .and_then(|record| record.resolved_directory())
        else {
            return;
        };
        let id = session.clone();
        let finished = key.clone();
        let started = self.spawn_session_task(
            move |client| Box::pin(generate_and_save(client, id, directory)),
            move |this, result, window, cx| {
                this.session_management.ai_rename.pending.remove(&finished);
                if finished.runtime != this.runtime {
                    return;
                }
                match result {
                    Some(Ok(Some(record))) => {
                        let event = ServerEvent::SessionPatched {
                            session_id: finished.session.clone(),
                            patch: SessionPatch {
                                title: Some(record.title),
                                ..SessionPatch::default()
                            },
                        };
                        this.apply_server_events(vec![event], window, cx);
                    }
                    // Another change won; the session keeps it.
                    Some(Ok(None)) | None => {}
                    Some(Err(error)) => {
                        tracing::warn!(?error, "the session could not be renamed with AI");
                        this.push_error("sessions.aiRename.failed", None, window, cx);
                    }
                }
            },
            window,
            cx,
        );
        if started {
            self.session_management.ai_rename.pending.insert(key);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_switch_drops_the_menu_check() {
        let mut rename = AiRename::default();
        rename.pending.insert(SessionKey {
            runtime: openchamber_native_core::ids::RuntimeId::from("a"),
            session: SessionId::from("s"),
        });
        rename.switch_runtime();
        assert!(rename.check.is_none());
        assert_eq!(rename.pending.len(), 1, "a rename in flight finishes");
    }
}
