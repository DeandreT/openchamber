//! Connected text send and named-session abort orchestration.
//!
//! Tasks belong to the window, survive selection/runtime switches and always
//! complete against their captured owner. No mutation is retried.

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Window};
use openchamber_native_core::draft::{Draft, DraftTarget};
use openchamber_native_core::ids::{PartId, SessionId};
use openchamber_native_core::transcript::{Message, MessageInfo, Part, Role, TranscriptEvent};
use openchamber_native_transport::prompt::{
    AbortError, PromptAdmission, PromptReceipt, SendError, TextPrompt,
};

use crate::connection::ConnectionState;
use crate::locale;
use crate::send_attempt::{self, Admission, SendAttempt, SessionKey};
use crate::server_sync::apply_list_change;
use crate::workspace::Workspace;

/// Public activation waits for admission-unknown recovery copy and runtime
/// qualification. Tests call the connected methods directly.
pub(crate) const CONNECTED_MUTATIONS_ENABLED: bool = false;

impl Workspace {
    pub(crate) fn mutation_key(&self, session: &SessionId) -> SessionKey {
        SessionKey {
            runtime: self.runtime.clone(),
            session: session.clone(),
        }
    }

    pub(crate) fn can_send_connected(&self) -> bool {
        CONNECTED_MUTATIONS_ENABLED
            && self
                .selected
                .as_ref()
                .is_some_and(|session| self.send_attempts.can_send(&self.mutation_key(session)))
            && self.connection.as_ref().is_some_and(|connection| {
                matches!(connection.state, ConnectionState::Connected)
                    && connection.client.is_some()
            })
    }

    pub(crate) fn submit_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        if !self.send_attempts.can_send(&key) || self.is_streaming(&session) {
            return;
        }
        let Some(owner) = self.composer_key.clone().filter(|owner| {
            owner.runtime() == &key.runtime
                && owner.target() == &DraftTarget::Session(session.clone())
        }) else {
            return;
        };
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        if selection.model.is_none() {
            return;
        }
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Some(open) = self.open_sessions.get_mut(&session) else {
            return;
        };
        let composer = self.composer.read(cx);
        let draft = Draft {
            text: composer.value().to_string(),
            selection: composer.selected_range(),
        };
        if draft.text.trim().is_empty() {
            return;
        }
        let (message_id, created) = self.message_ids.next();
        let prompt = TextPrompt {
            message_id: message_id.clone(),
            text: draft.text.clone(),
            model: selection.model,
            agent: selection.agent_name,
            delivery: None,
        };
        let optimistic = Message {
            info: MessageInfo {
                id: message_id.clone(),
                role: Role::User,
                created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: vec![Part::text(
                PartId::new(format!("{message_id}-local-text")),
                draft.text.clone(),
            )],
        };
        let attempt = SendAttempt {
            owner: owner.clone(),
            draft,
            prompt: prompt.clone(),
            optimistic: optimistic.clone(),
            admission: Admission::Sending { observed: false },
        };
        if !self.send_attempts.begin(key.clone(), attempt) {
            return;
        }
        let change = open.transcript.push(optimistic);
        apply_list_change(open, &change, cx);
        open.list.scroll_to_end();
        let request_session = session.clone();
        let request = connection
            .network
            .run(async move { client.send_text(&request_session, &prompt).await });
        self.drafts.take(&owner);
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_send(&key, result, window, cx);
                this.mutation_tasks.remove(&key);
            });
        });
        self.mutation_tasks.insert(task_key, task);
        cx.notify();
    }

    pub(crate) fn finish_send(
        &mut self,
        key: &SessionKey,
        result: Option<Result<PromptReceipt, SendError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A lost task result cannot establish that the server rejected a POST.
        let admission = match result {
            Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(error.admission),
            None => Err(PromptAdmission::Unknown),
        };
        let failed = self.send_attempts.finish(key, admission);
        if let Some(attempt) = failed {
            if self.runtime == key.runtime
                && let Some(open) = self.open_sessions.get_mut(&key.session)
            {
                let change = open.transcript.apply(TranscriptEvent::MessageRemoved {
                    message_id: attempt.prompt.message_id,
                });
                apply_list_change(open, &change, cx);
            }
            self.save_composer_draft(cx);
            let draft =
                send_attempt::restore_failed(self.drafts.get(&attempt.owner), attempt.draft);
            self.drafts.save(attempt.owner.clone(), draft);
            if self.composer_key.as_ref() == Some(&attempt.owner) {
                self.restore_composer_draft(window, cx);
            }
            self.notify_mutation_failure(window, cx);
        } else if self.runtime == key.runtime {
            if admission == Err(PromptAdmission::Unknown)
                && self.send_attempts.attempt(key).is_some()
            {
                self.notify_mutation_failure(window, cx);
            }
            // The existing read path merges exact IDs. An absent ID leaves the
            // attempt locked and preserved; history never infers rejection.
            self.load_history(&key.session, cx);
        }
        cx.notify();
    }

    pub(crate) fn abort_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        if !self.send_attempts.begin_abort(key.clone()) {
            return;
        }
        let request = connection
            .network
            .run(async move { client.abort_session(&session).await });
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_abort(&key, result, window, cx);
                this.mutation_tasks.remove(&key);
            });
        });
        self.mutation_tasks.insert(task_key, task);
        cx.notify();
    }

    pub(crate) fn finish_abort(
        &mut self,
        key: &SessionKey,
        result: Option<Result<bool, AbortError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send_attempts.finish_abort(key);
        // Both successful admission and abort leave live status to the
        // server's status stream. Failure cannot manufacture idle.
        if !matches!(result, Some(Ok(_))) {
            self.notify_mutation_failure(window, cx);
        }
        cx.notify();
    }

    fn notify_mutation_failure(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(
            Notification::error(locale::strings(cx).get("chat.errorBoundary.title")),
            cx,
        );
    }
}
