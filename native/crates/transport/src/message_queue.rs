//! The server-owned message queue (`/api/message-queue`), as
//! `stores/messageQueueStore.ts` reads and changes it.
//!
//! Every answer carries the revision of the queue it describes; the core's
//! projection applies it. A session's queue belongs to the server that sees
//! its OpenCode go idle: a directory inside an isolated space travels in the
//! `x-opencode-directory` header, which sends the request to that space's
//! server, as `runtimeFetch` does. Snapshots and broadcasts leave out the
//! payloads (attachment data and context); a take returns them.

use crate::request::RequestBuilder;
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::message_queue::{
    AttachmentSource, QueueSession, QueuedAttachment, QueuedItemId, QueuedMessage,
    QueuedSendConfig, queue_owner,
};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory, read_json};
use crate::error::TransportError;
use crate::prompt::{Delivery, Prompt};
use crate::prompt_parts::{AgentMention, Metadata, PromptFile, PromptParts, SyntheticContext};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendConfigWire {
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    agent: Option<String>,
    variant: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentWire {
    filename: String,
    size: f64,
    source: String,
    server_path: Option<String>,
    data_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ContextWire {
    Context {
        text: String,
        metadata: Map<String, Value>,
        instructions: Option<String>,
    },
    Instruction {
        text: String,
    },
    Synthetic {
        text: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemWire {
    id: String,
    created_at: f64,
    content: String,
    text: String,
    agent_mention: Option<String>,
    attachments: Vec<AttachmentWire>,
    context: Option<Vec<ContextWire>>,
    context_preview: Option<String>,
    send_config: SendConfigWire,
    goal: Option<Value>,
    scheduled_task: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionWire {
    session_id: String,
    directory: String,
    items: Vec<ItemWire>,
    sending_id: Option<String>,
}

#[derive(Deserialize)]
struct SnapshotWire {
    revision: f64,
    sessions: Vec<SessionWire>,
}

#[derive(Deserialize)]
struct SessionResponseWire {
    revision: f64,
    session: SessionWire,
}

#[derive(Deserialize)]
struct TakeWire {
    revision: f64,
    session: SessionWire,
    item: ItemWire,
}

#[derive(Deserialize)]
struct TakeAllWire {
    revision: f64,
    session: SessionWire,
    items: Vec<ItemWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReservationWire {
    id: String,
    item_ids: Vec<String>,
    composer: String,
    send_config: SendConfigWire,
    delivery: Option<String>,
}

#[derive(Deserialize)]
struct ReservedWire {
    revision: f64,
    session: SessionWire,
    reservation: ReservationWire,
    items: Vec<ItemWire>,
}

/// An immutable server-owned prefix and its stable prompt identity. Until
/// that prompt is observed admitted, the owner retains the items and excludes
/// every later delivery. Repeating a reservation recovers the same receipt.
pub struct ReservedPrefix {
    pub update: QueueUpdate,
    pub id: MessageId,
    pub composer: String,
    pub send_config: QueuedSendConfig,
    pub steer: bool,
    pub items: Vec<TakenMessage>,
}

impl ReservedPrefix {
    /// Reconstructs the exact request whose identity reserves this prefix.
    /// Context IDs are deterministic so preview, restart and reservation agree.
    pub fn prompt(&self) -> Prompt {
        let mut parts = PromptParts::default();
        let mut texts = Vec::new();
        let mut index = 0;
        for item in &self.items {
            if let Some(name) = &item.message.agent_mention
                && parts.agents.is_empty()
            {
                parts.agents.push(AgentMention {
                    name: name.clone(),
                    mention: None,
                });
            }
            if !item.message.text.trim().is_empty() {
                texts.push(item.message.text.clone());
            }
            parts.files.extend(item.files.iter().cloned());
            parts.context.extend(item.synthetic_context(|| {
                let id = MessageId::new(format!("{}-context-{index}", self.id));
                index += 1;
                id
            }));
        }
        if !self.composer.trim().is_empty() {
            texts.push(self.composer.clone());
        }
        Prompt {
            message_id: self.id.clone(),
            text: texts.join("\n\n"),
            parts,
            model: Some(openchamber_native_core::send_config::ModelChoice {
                provider_id: self.send_config.provider_id.clone(),
                model_id: self.send_config.model_id.clone(),
                variant: self.send_config.variant.clone(),
            }),
            agent: self.send_config.agent.clone(),
            delivery: self.steer.then_some(Delivery::Steer),
        }
    }
}

impl std::fmt::Debug for ReservedPrefix {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReservedPrefix")
            .field("id", &self.id)
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

/// Every queue one owner holds, at one revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub revision: u64,
    pub sessions: Vec<QueueSession>,
}

/// One session's queue after a change, at the change's revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueUpdate {
    pub revision: u64,
    pub session: QueueSession,
}

/// Context a taken message was queued with.
#[derive(Clone, PartialEq, Eq)]
pub enum TakenContext {
    /// An attached item; its `instructions`, when present, go just before it.
    Context {
        text: String,
        metadata: Metadata,
        instructions: Option<String>,
    },
    /// Derived from the text or handed over by another surface.
    Text(String),
}

impl std::fmt::Debug for TakenContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TakenContext([redacted])")
    }
}

/// A message taken out of the queue with its payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakenMessage {
    pub message: QueuedMessage,
    /// The attachments as the prompt references them.
    pub files: Vec<PromptFile>,
    pub context: Vec<TakenContext>,
}

impl TakenMessage {
    /// The synthetic context delivered before the prompt, in the order
    /// `queuedContextToParts` gives: an item's instructions, then the item.
    /// `next_id` mints each item's identity in send order.
    pub fn synthetic_context(
        &self,
        mut next_id: impl FnMut() -> MessageId,
    ) -> Vec<SyntheticContext> {
        let mut context = Vec::new();
        let mut push = |text: &str, metadata: Option<Metadata>| {
            context.push(SyntheticContext {
                id: next_id(),
                text: text.to_owned(),
                description: None,
                metadata,
            });
        };
        for part in &self.context {
            match part {
                TakenContext::Context {
                    text,
                    metadata,
                    instructions,
                } => {
                    if let Some(instructions) =
                        instructions.as_deref().filter(|text| !text.is_empty())
                    {
                        push(instructions, None);
                    }
                    push(text, Some(metadata.clone()));
                }
                TakenContext::Text(text) => push(text, None),
            }
        }
        context
    }
}

/// What the composer queues: its text and the selection it would send with.
/// The native composer has no attachments or context chips, so neither rides
/// along.
#[derive(Clone, PartialEq, Eq)]
pub struct QueueItemInput {
    /// The typed text, for display and editing.
    pub content: String,
    /// The text to deliver.
    pub text: String,
    pub send_config: QueuedSendConfig,
}

impl std::fmt::Debug for QueueItemInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueueItemInput")
            .field("send_config", &self.send_config.provider_id)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SendConfigBody<'a> {
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    #[serde(rename = "modelID")]
    model_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<&'a str>,
}

fn revision(value: f64) -> Result<u64, TransportError> {
    if value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64 {
        Ok(value as u64)
    } else {
        Err(TransportError::Malformed(
            "invalid queue revision".to_owned(),
        ))
    }
}

/// `encodeURIComponent` of a path segment, for `file://` URLs.
fn file_url(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim();
    if normalized.to_ascii_lowercase().starts_with("file://") {
        return normalized.to_owned();
    }
    let rooted = if normalized.as_bytes().get(1) == Some(&b':')
        && normalized.as_bytes()[0].is_ascii_alphabetic()
    {
        format!("/{normalized}")
    } else {
        normalized.to_owned()
    };
    let segments: Vec<String> = rooted
        .split('/')
        .enumerate()
        .map(|(index, segment)| {
            let drive = index == 1
                && segment.len() == 2
                && segment.as_bytes()[1] == b':'
                && segment.as_bytes()[0].is_ascii_alphabetic();
            if drive {
                segment.to_owned()
            } else {
                encode_directory(segment)
            }
        })
        .collect();
    format!("file://{}", segments.join("/"))
}

fn send_config(
    wire: SendConfigWire,
    scheduled: bool,
) -> Result<Option<QueuedSendConfig>, TransportError> {
    let nonblank = |value: Option<String>| value.filter(|value| !value.is_empty());
    match (nonblank(wire.provider_id), nonblank(wire.model_id)) {
        (Some(provider_id), Some(model_id)) => Ok(Some(QueuedSendConfig {
            provider_id,
            model_id,
            agent: wire.agent,
            variant: wire.variant,
        })),
        // Only a scheduled item may keep the session's selection.
        _ if scheduled => Ok(None),
        _ => Err(TransportError::Malformed(
            "queued item without a model".to_owned(),
        )),
    }
}

fn taken(wire: ItemWire) -> Result<TakenMessage, TransportError> {
    let files = wire
        .attachments
        .iter()
        .map(|attachment| {
            let uri = match (&attachment.server_path, attachment.source.as_str()) {
                (Some(path), "server") if !path.is_empty() => file_url(path),
                _ => attachment.data_url.clone().unwrap_or_default(),
            };
            PromptFile {
                uri,
                name: Some(attachment.filename.clone()),
            }
        })
        .collect();
    let context = wire
        .context
        .iter()
        .flatten()
        .map(|part| match part {
            ContextWire::Context {
                text,
                metadata,
                instructions,
            } => TakenContext::Context {
                text: text.clone(),
                metadata: Metadata(metadata.clone()),
                instructions: instructions.clone(),
            },
            ContextWire::Instruction { text } | ContextWire::Synthetic { text } => {
                TakenContext::Text(text.clone())
            }
        })
        .collect();
    Ok(TakenMessage {
        message: message(wire)?,
        files,
        context,
    })
}

fn message(wire: ItemWire) -> Result<QueuedMessage, TransportError> {
    if wire.id.is_empty() || !wire.created_at.is_finite() {
        return Err(TransportError::Malformed("invalid queued item".to_owned()));
    }
    let mut attachments = Vec::with_capacity(wire.attachments.len());
    for attachment in &wire.attachments {
        let source = match attachment.source.as_str() {
            "local" => AttachmentSource::Local,
            "server" => AttachmentSource::Server,
            "vscode" => AttachmentSource::Vscode,
            _ => {
                return Err(TransportError::Malformed(
                    "invalid queued attachment".to_owned(),
                ));
            }
        };
        if !attachment.size.is_finite() || attachment.size < 0.0 {
            return Err(TransportError::Malformed(
                "invalid queued attachment".to_owned(),
            ));
        }
        attachments.push(QueuedAttachment {
            filename: attachment.filename.clone(),
            source,
            size: attachment.size.ceil() as u64,
            server_path: attachment
                .server_path
                .clone()
                .filter(|path| !path.is_empty()),
        });
    }
    let scheduled = wire.scheduled_task.is_some();
    Ok(QueuedMessage {
        id: QueuedItemId::new(wire.id),
        content: wire.content,
        text: wire.text,
        agent_mention: wire.agent_mention.filter(|name| !name.is_empty()),
        attachments,
        has_context: wire
            .context
            .as_ref()
            .is_some_and(|context| !context.is_empty())
            || wire
                .context_preview
                .as_ref()
                .is_some_and(|preview| !preview.is_empty()),
        context_preview: wire.context_preview.filter(|preview| !preview.is_empty()),
        created_at: wire.created_at as i64,
        send_config: send_config(wire.send_config, scheduled)?,
        goal: wire.goal.as_ref().is_some_and(Value::is_object),
        scheduled,
    })
}

pub(crate) fn queue_session(wire: SessionWire) -> Result<QueueSession, TransportError> {
    if wire.session_id.is_empty() {
        return Err(TransportError::Malformed(
            "queue without a session".to_owned(),
        ));
    }
    Ok(QueueSession {
        session: SessionId::new(wire.session_id),
        directory: wire.directory,
        items: wire
            .items
            .into_iter()
            .map(message)
            .collect::<Result<_, _>>()?,
        sending: wire
            .sending_id
            .filter(|id| !id.is_empty())
            .map(QueuedItemId::new),
    })
}

/// A `openchamber:message-queue.updated` broadcast: the session's queue, its
/// revision, and the space it came from when the host forwarded it.
pub(crate) fn decode_update(properties: &Value) -> Option<(QueueUpdate, Option<String>)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UpdateWire {
        revision: f64,
        session: SessionWire,
        space_id: Option<String>,
    }
    let wire: UpdateWire = serde_json::from_value(properties.clone()).ok()?;
    let update = QueueUpdate {
        revision: revision(wire.revision).ok()?,
        session: queue_session(wire.session).ok()?,
    };
    Some((update, wire.space_id))
}

impl ServerClient {
    pub fn message_queue_url(&self) -> Url {
        self.endpoint().url("api/message-queue")
    }

    fn queue_session_url(&self, session: &SessionId, tail: &[&str]) -> Url {
        let mut url = self.endpoint().url("api/message-queue/sessions/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("http and https URLs have path segments");
            segments.pop_if_empty().push(session.as_str());
            for segment in tail {
                segments.push(segment);
            }
        }
        url
    }

    /// A request about a queue, sent to the server that owns `directory`.
    fn queue_request(&self, method: Method, url: Url, directory: &str) -> RequestBuilder {
        let request = self.authorized(method, url);
        if queue_owner(directory).is_some() {
            request
                .header("x-opencode-directory", encode_directory(directory))
                .header("x-opencode-directory-encoding", "uri")
        } else {
            request
        }
    }

    async fn queue_session_call(
        &self,
        request: RequestBuilder,
    ) -> Result<QueueUpdate, TransportError> {
        let response = request.send().await.map_err(TransportError::from_request)?;
        let wire: SessionResponseWire = read_json(response).await?;
        Ok(QueueUpdate {
            revision: revision(wire.revision)?,
            session: queue_session(wire.session)?,
        })
    }

    /// Every queue of the host, or of the space whose ID is `space`. A
    /// failure is an error, never an empty queue.
    pub async fn message_queue(
        &self,
        space: Option<&str>,
    ) -> Result<QueueSnapshot, TransportError> {
        let directory = space.map(|space| format!("/spaces/{space}"));
        let request = match &directory {
            Some(directory) => self.queue_request(Method::GET, self.message_queue_url(), directory),
            None => self.authorized(Method::GET, self.message_queue_url()),
        };
        let response = request.send().await.map_err(TransportError::from_request)?;
        let wire: SnapshotWire = read_json(response).await?;
        Ok(QueueSnapshot {
            revision: revision(wire.revision)?,
            sessions: wire
                .sessions
                .into_iter()
                .map(queue_session)
                .collect::<Result<_, _>>()?,
        })
    }

    /// Appends a message to `session`'s queue in `directory`. The server
    /// may deliver it at once when the session is already idle.
    pub async fn enqueue_message(
        &self,
        session: &SessionId,
        directory: &str,
        item: &QueueItemInput,
    ) -> Result<QueueUpdate, TransportError> {
        let config = &item.send_config;
        let body = json!({
            "directory": directory,
            "item": {
                "content": item.content,
                "text": item.text,
                "attachments": [],
                "context": [],
                "sendConfig": SendConfigBody {
                    provider_id: &config.provider_id,
                    model_id: &config.model_id,
                    agent: config.agent.as_deref(),
                    variant: config.variant.as_deref(),
                },
            },
        });
        let request = self
            .queue_request(
                Method::POST,
                self.queue_session_url(session, &["items"]),
                directory,
            )
            .json(&body);
        self.queue_session_call(request).await
    }

    /// Removes one queued message; `Http { status: 409 }` while it is being
    /// delivered.
    pub async fn remove_queued(
        &self,
        session: &SessionId,
        directory: &str,
        item: &QueuedItemId,
    ) -> Result<QueueUpdate, TransportError> {
        let url = self.queue_session_url(session, &["items", item.as_str()]);
        self.queue_session_call(self.queue_request(Method::DELETE, url, directory))
            .await
    }

    /// Puts the queue in `order`, a complete permutation of its items.
    pub async fn reorder_queue(
        &self,
        session: &SessionId,
        directory: &str,
        order: &[QueuedItemId],
    ) -> Result<QueueUpdate, TransportError> {
        let ids: Vec<&str> = order.iter().map(QueuedItemId::as_str).collect();
        let request = self
            .queue_request(
                Method::PUT,
                self.queue_session_url(session, &["order"]),
                directory,
            )
            .json(&json!({ "itemIds": ids }));
        self.queue_session_call(request).await
    }

    /// Removes one message and returns it with its payloads; 404 when it is
    /// gone, 409 while it is being delivered.
    pub async fn take_queued(
        &self,
        session: &SessionId,
        directory: &str,
        item: &QueuedItemId,
    ) -> Result<(QueueUpdate, TakenMessage), TransportError> {
        let url = self.queue_session_url(session, &["items", item.as_str(), "take"]);
        let response = self
            .queue_request(Method::POST, url, directory)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let wire: TakeWire = read_json(response).await?;
        Ok((
            QueueUpdate {
                revision: revision(wire.revision)?,
                session: queue_session(wire.session)?,
            },
            taken(wire.item)?,
        ))
    }

    /// Removes and returns every message not being delivered, in order.
    /// Scheduled items stay queued.
    pub async fn take_all_queued(
        &self,
        session_id: &SessionId,
        directory: &str,
    ) -> Result<(QueueUpdate, Vec<TakenMessage>), TransportError> {
        let url = self.queue_session_url(session_id, &["take"]);
        let response = self
            .queue_request(Method::POST, url, directory)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let wire: TakeAllWire = read_json(response).await?;
        Ok((
            QueueUpdate {
                revision: revision(wire.revision)?,
                session: queue_session(wire.session)?,
            },
            wire.items
                .into_iter()
                .map(taken)
                .collect::<Result<_, _>>()?,
        ))
    }

    /// Reserves exactly the requested leading items, without removing them.
    /// A server without this explicit operation refuses; there is no unsafe
    /// fallback to individual takes or the legacy unbounded take-all.
    pub async fn reserve_queued_prefix(
        &self,
        session: &SessionId,
        directory: &str,
        id: &MessageId,
        items: &[QueuedItemId],
        input: &QueueItemInput,
        steer: bool,
    ) -> Result<ReservedPrefix, TransportError> {
        self.queued_prefix(
            (session, directory),
            id,
            items,
            input,
            steer,
            "reserve-prefix",
        )
        .await
    }

    /// Reads the immutable candidate payload without acquiring server ownership.
    /// The caller journals the whole prompt before requesting reservation.
    pub async fn preview_queued_prefix(
        &self,
        session: &SessionId,
        directory: &str,
        id: &MessageId,
        items: &[QueuedItemId],
        input: &QueueItemInput,
        steer: bool,
    ) -> Result<ReservedPrefix, TransportError> {
        self.queued_prefix(
            (session, directory),
            id,
            items,
            input,
            steer,
            "preview-prefix",
        )
        .await
    }

    async fn queued_prefix(
        &self,
        (session, directory): (&SessionId, &str),
        id: &MessageId,
        items: &[QueuedItemId],
        input: &QueueItemInput,
        steer: bool,
        operation: &str,
    ) -> Result<ReservedPrefix, TransportError> {
        let config = &input.send_config;
        let ids: Vec<&str> = items.iter().map(QueuedItemId::as_str).collect();
        let response = self
            .queue_request(
                Method::POST,
                self.queue_session_url(session, &[operation]),
                directory,
            )
            .json(&json!({
                "id": id.as_str(), "itemIds": ids, "composer": input.text,
                "delivery": if steer { Some("steer") } else { None },
                "sendConfig": SendConfigBody {
                    provider_id: &config.provider_id, model_id: &config.model_id,
                    agent: config.agent.as_deref(), variant: config.variant.as_deref(),
                },
            }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let mut response = response;
        if operation == "reserve-prefix" && response.status() == reqwest::StatusCode::CONFLICT {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Unavailable {
                code: String,
                #[serde(rename = "sessionID")]
                session_id: String,
                #[serde(rename = "messageID")]
                message_id: String,
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(TransportError::from_request)?
            {
                if bytes.len() + chunk.len() > 4096 {
                    return Err(TransportError::Http { status: 409 });
                }
                bytes.extend_from_slice(&chunk);
            }
            if serde_json::from_slice::<Unavailable>(&bytes).is_ok_and(|error| {
                error.code == "queue_prefix_unavailable"
                    && error.session_id == session.as_str()
                    && error.message_id == id.as_str()
            }) {
                return Err(TransportError::QueuePrefixUnavailable);
            }
            return Err(TransportError::Http { status: 409 });
        }
        let wire: ReservedWire = read_json(response).await?;
        let receipt = reserved_prefix(wire, session)?;
        if openchamber_native_core::path::normalize(directory).as_deref()
            != Some(receipt.update.session.directory.as_str())
        {
            return Err(TransportError::Malformed(
                "queue reservation changed its directory".into(),
            ));
        }
        Ok(receipt)
    }
}

fn reserved_prefix(
    wire: ReservedWire,
    session: &SessionId,
) -> Result<ReservedPrefix, TransportError> {
    let receipt = wire.reservation;
    if wire.session.session_id != session.as_str()
        || !receipt.id.starts_with("msg_")
        || receipt.id.len() <= 4
        || receipt.id.len() > 124
        || !receipt
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || receipt.item_ids.is_empty()
        || receipt.item_ids.len() != wire.items.len()
        || wire
            .items
            .iter()
            .zip(&receipt.item_ids)
            .any(|(item, id)| &item.id != id)
        || !matches!(receipt.delivery.as_deref(), None | Some("steer"))
    {
        return Err(TransportError::Malformed(
            "invalid queue reservation".into(),
        ));
    }
    let items = wire
        .items
        .into_iter()
        .map(taken)
        .collect::<Result<Vec<_>, _>>()?;
    if items.iter().any(|item| {
        !item.message.takeable(64 * 1024)
            || item.files.iter().any(|file| file.uri.len() > 64 * 1024)
    }) {
        return Err(TransportError::Malformed(
            "unusable queue reservation".into(),
        ));
    }
    Ok(ReservedPrefix {
        update: QueueUpdate {
            revision: revision(wire.revision)?,
            session: queue_session(wire.session)?,
        },
        id: MessageId::new(receipt.id),
        composer: receipt.composer,
        send_config: send_config(receipt.send_config, false)?.expect("required selection"),
        steer: receipt.delivery.as_deref() == Some("steer"),
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(extra: &str) -> String {
        format!(
            r#"{{"id":"q1","createdAt":5,"content":"hi","text":"hi","attachments":[],"sendConfig":{{"providerID":"p","modelID":"m","agent":"build"}}{extra}}}"#
        )
    }

    #[test]
    fn prefix_receipts_keep_identity_and_reject_incomplete_or_wrong_owner_answers() {
        let queued: Value = serde_json::from_str(&item("")).expect("item");
        let fixture = json!({
            "revision": 9,
            "session": {"sessionId":"ses_1", "directory":"/repo", "items":[queued.clone()], "sendingId":null},
            "reservation": {"id":"msg_reserved", "itemIds":["q1"], "composer":"typed",
                "sendConfig":{"providerID":"p", "modelID":"m"}, "delivery":"steer"},
            "items":[queued],
        });
        let parse = |value| {
            reserved_prefix(
                serde_json::from_value(value).expect("wire"),
                &SessionId::new("ses_1"),
            )
        };
        let receipt = parse(fixture.clone()).expect("valid receipt");
        assert_eq!(receipt.id.as_str(), "msg_reserved");
        assert_eq!(receipt.composer, "typed");
        assert!(receipt.steer);
        assert!(
            !format!("{receipt:?}").contains("typed"),
            "debug omits conversation content"
        );
        let mut missing = fixture.clone();
        missing["items"] = json!([]);
        assert!(parse(missing).is_err());
        let mut wrong_owner = fixture.clone();
        wrong_owner["session"]["sessionId"] = json!("ses_other");
        assert!(parse(wrong_owner).is_err());
        let mut wrong_order = fixture;
        wrong_order["reservation"]["itemIds"] = json!(["q2"]);
        assert!(parse(wrong_order).is_err());
    }

    #[test]
    fn decodes_a_broadcast_with_its_space() {
        let properties: Value = serde_json::from_str(&format!(
            r#"{{"revision":7,"spaceId":"0123456789ab","session":{{"sessionId":"ses_1","directory":"/spaces/0123456789ab/r","items":[{}],"sendingId":null}}}}"#,
            item("")
        ))
        .expect("fixture is JSON");
        let (update, space) = decode_update(&properties).expect("valid broadcast");
        assert_eq!(update.revision, 7);
        assert_eq!(space.as_deref(), Some("0123456789ab"));
        let queued = &update.session.items[0];
        assert_eq!(
            queued.send_config.as_ref().and_then(|c| c.agent.as_deref()),
            Some("build")
        );
        assert!(queued.editable_as_text());
    }

    #[test]
    fn rejects_items_that_could_not_be_delivered() {
        for bad in [
            r#"{"id":"","createdAt":1,"content":"","text":"","attachments":[],"sendConfig":{"providerID":"p","modelID":"m"}}"#,
            r#"{"id":"q","createdAt":1,"content":"","text":"","attachments":[],"sendConfig":{}}"#,
        ] {
            let wire: ItemWire = serde_json::from_str(bad).expect("fixture is JSON");
            assert!(message(wire).is_err());
        }
        let scheduled: ItemWire = serde_json::from_str(
            r#"{"id":"q","createdAt":1,"content":"","text":"x","attachments":[],"sendConfig":{},"scheduledTask":{"projectId":"p","taskId":"t"}}"#,
        )
        .expect("fixture is JSON");
        let scheduled = message(scheduled).expect("a scheduled item keeps the session's model");
        assert!(scheduled.scheduled && scheduled.send_config.is_none());
        assert!(revision(-1.0).is_err() && revision(1.5).is_err());
    }

    #[test]
    fn a_taken_item_delivers_its_payloads_in_order() {
        let wire: ItemWire = serde_json::from_str(&item(
            r#","attachments":[{"id":"a","filename":"my file.png","mimeType":"image/png","size":3,"source":"server","serverPath":"C:\\work\\my file.png"},{"id":"b","filename":"x.txt","mimeType":"text/plain","size":1,"source":"local","dataUrl":"data:text/plain,x"}],"context":[{"kind":"context","text":"ctx","metadata":{"openchamberContext":{}},"instructions":"look"},{"kind":"instruction","text":"skill"}]"#,
        ).replacen(r#""attachments":[],"#, "", 1))
        .expect("fixture is JSON");
        let taken = taken(wire).expect("valid item");
        let kept = &taken.message.attachments;
        assert_eq!(kept[0].source, AttachmentSource::Server);
        assert_eq!(
            kept[0].server_path.as_deref(),
            Some("C:\\work\\my file.png")
        );
        assert_eq!((kept[1].source, kept[1].size), (AttachmentSource::Local, 1));
        assert_eq!(taken.files[0].uri, "file:///C:/work/my%20file.png");
        assert_eq!(taken.files[1].uri, "data:text/plain,x");
        let mut next = 0;
        let context = taken.synthetic_context(|| {
            next += 1;
            MessageId::new(format!("msg_{next}"))
        });
        let texts: Vec<&str> = context.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(texts, ["look", "ctx", "skill"]);
        assert!(context[1].metadata.is_some() && context[0].metadata.is_none());
        assert!(!taken.message.editable_as_text());
    }
}
