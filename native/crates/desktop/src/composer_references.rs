//! Sessions and messages the composer's text links to (`sessionReferences.ts`
//! and ChatInput's `readSessionMentionsBeforeSend`).
//!
//! The text decides what goes out. Before a send each linked session or
//! message is read once, a session from the session index and a message
//! whole from its session; a send waits for that read, and a second send
//! while it runs does nothing. Each one the read found then joins the
//! draft as a context item, in the order its link first appears, so the
//! existing rich send delivers it as the synthetic context the web client
//! sends. A link this server cannot resolve goes as the text it is.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft_content::DraftContext;
use openchamber_native_core::ids::{ContextItemId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::session_reference::{
    self, SessionLinkTarget, SessionReference, link_key,
};
use serde_json::{Value, json};

use crate::markdown_links;
use crate::network::NetworkTask;
use crate::workspace::Workspace;

/// Context items this module owns carry this prefix, then the link key.
const ITEM_PREFIX: &str = "session-reference:";

type ReferenceKey = (RuntimeId, String);

/// What a send does again once the links are read.
pub(crate) type Resume = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// The references read so far, per runtime and link.
#[derive(Default)]
pub(crate) struct SessionReferences {
    resolved: HashMap<ReferenceKey, SessionReference>,
    /// Links read once already, found or not: one that failed is not read
    /// again on every send.
    attempted: HashSet<ReferenceKey>,
    read: Option<Task<()>>,
}

/// The JSON a reference's context part carries (`createContextPart`).
fn metadata(reference: &SessionReference) -> String {
    let payload = match reference {
        SessionReference::Session {
            session_id,
            title,
            message_id,
            ..
        } => {
            let mut payload = json!({ "kind": "session", "sessionId": session_id, "title": title });
            if let Some(message_id) = message_id {
                payload["messageId"] = Value::from(message_id.as_str());
            }
            payload
        }
        SessionReference::Message {
            session_id,
            message_id,
            session_title,
            excerpt,
            ..
        } => json!({
            "kind": "session-message",
            "sessionId": session_id,
            "messageId": message_id,
            "sessionTitle": session_title,
            "excerpt": excerpt,
        }),
    };
    json!({ "openchamberContext": payload }).to_string()
}

/// The context item a reference goes as.
pub(crate) fn context_item(reference: &SessionReference) -> DraftContext {
    DraftContext {
        id: ContextItemId::from(format!("{ITEM_PREFIX}{}", reference.key())),
        text: reference.context_text().to_owned(),
        description: None,
        metadata_json: Some(metadata(reference)),
        instructions: None,
    }
}

fn owned_item(item: &DraftContext) -> bool {
    item.id.as_str().starts_with(ITEM_PREFIX)
}

impl Workspace {
    /// A reference the `@` picker resolved as it wrote the link.
    pub(crate) fn remember_session_reference(&mut self, reference: SessionReference) {
        self.session_references
            .resolved
            .insert((self.runtime.clone(), reference.key()), reference);
    }

    /// Every session or message link in `text`, once each, in order.
    fn session_links(&self, text: &str) -> Vec<SessionLinkTarget> {
        let endpoint = self
            .connection
            .as_ref()
            .map(|connection| &connection.config.endpoint);
        let mut seen = HashSet::new();
        session_reference::link_candidates(text)
            .into_iter()
            .filter_map(|(_, link)| markdown_links::session_link_target(link, endpoint))
            .filter(|target| seen.insert(link_key(target)))
            .collect()
    }

    /// Whether the composer's text can be sent now. False while its links
    /// are read; `resume` runs once the read settles.
    pub(crate) fn session_references_ready(
        &mut self,
        resume: Resume,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_references.read.is_some() {
            return false;
        }
        let text = self.composer.read(cx).value().to_string();
        let runtime = self.runtime.clone();
        let pending: Vec<SessionLinkTarget> = self
            .session_links(&text)
            .into_iter()
            .filter(|target| {
                let key = (runtime.clone(), link_key(target));
                !self.session_references.resolved.contains_key(&key)
                    && !self.session_references.attempted.contains(&key)
            })
            .collect();
        if pending.is_empty() {
            self.attach_session_references(cx);
            return true;
        }
        let mut found = Vec::new();
        let mut reads = Vec::new();
        for target in &pending {
            self.session_references
                .attempted
                .insert((runtime.clone(), link_key(target)));
            let Some(session) = self
                .sessions
                .get(&SessionId::new(target.session_id.clone()))
            else {
                continue;
            };
            match &target.message_id {
                None => found.push(session_reference::session_reference(
                    &target.session_id,
                    &session.title,
                )),
                Some(message_id) => reads.push((
                    target.session_id.clone(),
                    session.title.clone(),
                    message_id.clone(),
                )),
            }
        }
        let client = self
            .connection
            .as_ref()
            .and_then(|connection| Some((connection.client.clone()?, connection.network.handle())));
        let total = pending.len();
        let Some((client, network)) = client.filter(|_| !reads.is_empty()) else {
            // Sessions resolve from the index; there is nothing to wait for.
            self.settle_session_references(&found, total, window, cx);
            self.attach_session_references(cx);
            return true;
        };
        self.session_references.read = Some(cx.spawn_in(window, async move |this, cx| {
            let mut found = found;
            {
                let read = NetworkTask::spawn_on(&network, async move {
                    let mut references = Vec::new();
                    for (session_id, title, message_id) in reads {
                        let message = client
                            .session_message(
                                &SessionId::new(session_id.clone()),
                                &MessageId::new(message_id.clone()),
                            )
                            .await;
                        if let Ok(message) = message {
                            references.push(
                                session_reference::message_reference(&session_id, &title, &message)
                                    .unwrap_or_else(|| {
                                        session_reference::session_reference_for_message(
                                            &session_id,
                                            &title,
                                            &message_id,
                                        )
                                    }),
                            );
                        }
                    }
                    references
                });
                found.extend(read.await.unwrap_or_default());
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.session_references.read = None;
                // A send on another server is not this one's to resume.
                if this.runtime != runtime {
                    return;
                }
                this.settle_session_references(&found, total, window, cx);
                resume(this, window, cx);
            });
        }));
        false
    }

    /// Keeps what a read found and says so when some link stays text.
    fn settle_session_references(
        &mut self,
        found: &[SessionReference],
        total: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for reference in found {
            self.remember_session_reference(reference.clone());
        }
        if found.len() < total {
            self.notify_mutation_failure("chat.sessionReference.unavailable", window, cx);
        }
    }

    /// Writes the text's references into the draft as context items,
    /// dropping the items of links no longer in it.
    fn attach_session_references(&mut self, cx: &mut Context<Self>) {
        let Some(owner) = self.composer_key.clone() else {
            return;
        };
        if self.composer_documents.contains(&owner) {
            return;
        }
        self.save_composer_draft(cx);
        let Some(mut draft) = self.drafts.get(&owner).cloned() else {
            return;
        };
        let references: Vec<DraftContext> = self
            .session_links(&draft.text)
            .iter()
            .filter_map(|target| {
                self.session_references
                    .resolved
                    .get(&(self.runtime.clone(), link_key(target)))
            })
            .map(context_item)
            .collect();
        let before = draft.content.context.clone();
        draft.content.context.retain(|item| !owned_item(item));
        draft.content.context.extend(references);
        if draft.content.context != before {
            self.drafts.save(owner, draft);
            self.queue_persistence(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_goes_as_context_with_its_payload() {
        let item = context_item(&session_reference::session_reference("ses_a", "Tweets"));
        assert_eq!(item.id.as_str(), "session-reference:ses_a:");
        assert!(item.text.contains("\"Tweets\" (ses_a)"));
        let metadata: Value =
            serde_json::from_str(item.metadata_json.as_deref().expect("metadata")).expect("JSON");
        assert_eq!(
            metadata,
            json!({ "openchamberContext": { "kind": "session", "sessionId": "ses_a", "title": "Tweets" } })
        );
        assert!(owned_item(&item));
        let named = context_item(&session_reference::session_reference_for_message(
            "ses_a", "Tweets", "msg_b",
        ));
        let metadata: Value =
            serde_json::from_str(named.metadata_json.as_deref().expect("metadata")).expect("JSON");
        assert_eq!(metadata["openchamberContext"]["messageId"], "msg_b");
    }
}
