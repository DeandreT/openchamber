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
/// The server sends a heartbeat well inside this interval.
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
const ROUTE: &str = "api/global/event/ws";

/// What the stream reports to its consumer.
#[derive(Debug)]
pub enum StreamItem {
    /// A connection is open. After a gap, `replay_reset` means missed events
    /// were not replayed and affected scopes need authoritative reads.
    Connected {
        replay_reset: bool,
    },
    Events(Vec<ServerEvent>),
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
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| match error {
            tokio_tungstenite::tungstenite::Error::Http(response)
                if matches!(response.status().as_u16(), 401 | 403) =>
            {
                TransportError::AuthenticationRequired
            }
            other => TransportError::Stream(other.to_string()),
        })?;
    loop {
        let message = match tokio::time::timeout(SILENCE_LIMIT, socket.next()).await {
            Err(_) => return Err(TransportError::Stream("the stream fell silent".to_owned())),
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
        if let Some(id) = frame.event_id {
            *last_event_id = Some(id);
        }
        let mut events = Vec::with_capacity(frame.events.len());
        for item in frame.events {
            match item {
                ServerEvent::Ready { replay_reset } => {
                    *failures = 0;
                    if sender
                        .send(StreamItem::Connected { replay_reset })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                other => events.push(other),
            }
        }
        if !events.is_empty() && sender.send(StreamItem::Events(events)).await.is_err() {
            return Ok(());
        }
    }
}
