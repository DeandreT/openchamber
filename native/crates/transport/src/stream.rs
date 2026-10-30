//! The global event stream with reconnects and replay.
//!
//! One task owns the WebSocket. It delivers decoded events through a bounded
//! channel and waits when the consumer falls behind, so no event is dropped
//! to make room. A dropped connection is reported, then retried with
//! exponential backoff from the last replay identifier. Dropping the
//! `EventStream` aborts the task and releases the subscription; it never
//! stops the server or a running turn.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::event::{self, ServerEvent};

const CHANNEL_CAPACITY: usize = 1024;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// The server sends a heartbeat well inside this interval
/// (`DEFAULT_HEARTBEAT_TIMEOUT_MS` in `sync/event-pipeline.ts`).
const SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// A socket that does not say `ready` this soon is retried
/// (`DEFAULT_WS_READY_TIMEOUT_MS`), as a server still starting OpenCode can
/// accept it and stay silent.
const READY_LIMIT: Duration = Duration::from_secs(2);
/// The longest a WebSocket handshake may take.
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);
const ROUTE: &str = "api/global/event/ws";

/// What the stream reports to its consumer.
#[derive(Debug)]
pub enum StreamItem {
    /// A connection is open. `resumed` means the server replayed every event
    /// since the last one delivered; otherwise anything missed while
    /// disconnected needs authoritative reads. `replay_through` names the
    /// last replayed event when a resumed replay is not empty: once an
    /// `Events` item with that id is applied, the replay is over.
    Connected {
        resumed: bool,
        replay_through: Option<String>,
    },
    /// One frame's events, with the frame's event id. A frame with an id is
    /// delivered even when none of its events decode, so a consumer can
    /// tell when that id was applied.
    Events {
        events: Vec<ServerEvent>,
        event_id: Option<String>,
    },
    /// The connection failed; live state is stale until the next `Connected`.
    Disconnected(TransportError),
}

/// A running subscription to the server's global events.
#[derive(Debug)]
pub struct EventStream {
    receiver: mpsc::Receiver<StreamItem>,
    task: JoinHandle<()>,
}

impl EventStream {
    /// Starts the subscription on the current Tokio runtime.
    pub fn subscribe(client: ServerClient) -> Self {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(run(client, sender));
        Self { receiver, task }
    }

    /// The next item, or `None` once the stream has ended.
    pub async fn next(&mut self) -> Option<StreamItem> {
        self.receiver.recv().await
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(client: ServerClient, sender: mpsc::Sender<StreamItem>) {
    let mut last_event_id: Option<String> = None;
    let mut failures: u32 = 0;
    loop {
        let outcome = connect_and_read(&client, &sender, &mut last_event_id, &mut failures).await;
        let Err(error) = outcome else {
            // The consumer went away.
            return;
        };
        let permanent = matches!(
            error,
            TransportError::AuthenticationRequired | TransportError::NotOpenChamber
        );
        if sender.send(StreamItem::Disconnected(error)).await.is_err() {
            return;
        }
        failures = failures.saturating_add(1);
        let exponent = failures.min(8);
        let mut delay = BACKOFF_BASE.saturating_mul(1 << exponent).min(BACKOFF_CAP);
        if permanent {
            // A credential problem will not fix itself quickly.
            delay = BACKOFF_CAP * 6;
        }
        tokio::time::sleep(delay).await;
    }
}

/// Reads one connection until it fails. `Ok` means the consumer is gone.
async fn connect_and_read(
    client: &ServerClient,
    sender: &mpsc::Sender<StreamItem>,
    last_event_id: &mut Option<String>,
    failures: &mut u32,
) -> Result<(), TransportError> {
    let mut url = client.endpoint().websocket_url(ROUTE);
    let cursor_sent = last_event_id.is_some();
    if let Some(id) = last_event_id.as_deref() {
        url.query_pairs_mut().append_pair("lastEventId", id);
    }
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|error| TransportError::Stream(error.to_string()))?;
    if let Some(value) = client.credential().authorization() {
        let mut value = HeaderValue::from_str(&value).map_err(|_| {
            TransportError::Stream("the credential is not a valid header".to_owned())
        })?;
        value.set_sensitive(true);
        request.headers_mut().insert("authorization", value);
    }
    let handshake =
        tokio::time::timeout(HANDSHAKE_LIMIT, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| TransportError::TimedOut)?;
    let (mut socket, _) = handshake.map_err(|error| match error {
        tokio_tungstenite::tungstenite::Error::Http(response)
            if matches!(response.status().as_u16(), 401 | 403) =>
        {
            TransportError::AuthenticationRequired
        }
        other => TransportError::Stream(other.to_string()),
    })?;
    let mut ready = false;
    loop {
        let limit = if ready { SILENCE_LIMIT } else { READY_LIMIT };
        let message = match tokio::time::timeout(limit, socket.next()).await {
            Err(_) if ready => {
                return Err(TransportError::Stream("the stream fell silent".to_owned()));
            }
            Err(_) => {
                return Err(TransportError::Stream(
                    "the server did not open the stream".to_owned(),
                ));
            }
            Ok(None) => {
                return Err(TransportError::Stream(
                    "the server closed the stream".to_owned(),
                ));
            }
            Ok(Some(Err(error))) => return Err(TransportError::Stream(error.to_string())),
            Ok(Some(Ok(message))) => message,
        };
        let text = match message {
            WsMessage::Text(text) => text,
            WsMessage::Close(_) => {
                return Err(TransportError::Stream(
                    "the server closed the stream".to_owned(),
                ));
            }
            _ => continue,
        };
        let frame = event::decode_frame(text.as_str())?;
        if let Some(id) = &frame.event_id {
            *last_event_id = Some(id.clone());
        }
        let mut events = Vec::with_capacity(frame.events.len());
        for item in frame.events {
            match item {
                ServerEvent::Ready {
                    replay_reset,
                    replay_through,
                } => {
                    ready = true;
                    *failures = 0;
                    if replay_reset {
                        // The cursor names nothing the server holds, such as an
                        // event from before a restart: retire it.
                        *last_event_id = None;
                    }
                    let resumed = cursor_sent && !replay_reset;
                    let replay_through = replay_through.filter(|_| resumed);
                    if sender
                        .send(StreamItem::Connected {
                            resumed,
                            replay_through,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                other => events.push(other),
            }
        }
        let event_id = frame.event_id;
        if (!events.is_empty() || event_id.is_some())
            && sender
                .send(StreamItem::Events { events, event_id })
                .await
                .is_err()
        {
            return Ok(());
        }
    }
}
