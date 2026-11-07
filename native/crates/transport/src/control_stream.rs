//! Authenticated OpenChamber control SSE, separate from the global event socket.
//!
//! Topology notifications have no replay. Every ready signal requires a topology
//! refresh. Dropping this subscription cancels its captured-server request.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::client::ServerClient;
use crate::error::TransportError;

const FRAME_LIMIT: usize = 1024 * 1024;
const READY_LIMIT: Duration = Duration::from_secs(10);
const SILENCE_LIMIT: Duration = Duration::from_secs(40);

/// Control events relevant to session topology.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlEvent {
    Connected,
    WorktreeChanged {
        directories: Vec<String>,
    },
    SessionCreated,
    /// A terminal was created, closed or exited on the server.
    TerminalSessionsChanged,
}

/// A control connection outcome. Failure never means empty topology.
#[derive(Debug)]
pub enum ControlItem {
    Event(ControlEvent),
    Disconnected(TransportError),
}

/// A single runtime's reconnecting, bounded control subscription.
#[derive(Debug)]
pub struct ControlStream {
    receiver: mpsc::Receiver<ControlItem>,
    task: JoinHandle<()>,
}

impl ControlStream {
    /// Starts on the active Tokio runtime using the captured authenticated client.
    pub fn subscribe(client: ServerClient) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        let task = tokio::spawn(run(client, sender));
        Self { receiver, task }
    }

    /// Waits for the next control event or connection failure.
    pub async fn next(&mut self) -> Option<ControlItem> {
        self.receiver.recv().await
    }
}

impl Drop for ControlStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(client: ServerClient, sender: mpsc::Sender<ControlItem>) {
    let mut failures = 0_u32;
    loop {
        let Err(error) = read_connection(&client, &sender, &mut failures).await else {
            return;
        };
        let permanent = matches!(
            error,
            TransportError::AuthenticationRequired | TransportError::Redirected
        ) || matches!(error, TransportError::Http { status } if (400..500).contains(&status) && !matches!(status, 408 | 429));
        if sender.send(ControlItem::Disconnected(error)).await.is_err() {
            return;
        }
        failures = failures.saturating_add(1);
        let delay = if permanent {
            Duration::from_secs(60)
        } else {
            Duration::from_millis(500)
                .saturating_mul(1 << failures.saturating_sub(1).min(6))
                .min(Duration::from_secs(30))
        };
        tokio::time::sleep(delay).await;
    }
}

async fn read_connection(
    client: &ServerClient,
    sender: &mpsc::Sender<ControlItem>,
    failures: &mut u32,
) -> Result<(), TransportError> {
    let response = client
        .authorized(Method::GET, client.endpoint().url("api/openchamber/events"))
        .header("accept", "text/event-stream")
        .timeout(Duration::from_secs(24 * 60 * 60))
        .send();
    let mut response = tokio::time::timeout(READY_LIMIT, response)
        .await
        .map_err(|_| TransportError::TimedOut)?
        .map_err(TransportError::from_request)?;
    match response.status() {
        StatusCode::UNAUTHORIZED => return Err(TransportError::AuthenticationRequired),
        status if status.is_redirection() => return Err(TransportError::Redirected),
        status if !status.is_success() => {
            return Err(TransportError::Http {
                status: status.as_u16(),
            });
        }
        _ => {}
    }
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return Err(TransportError::Stream("control response is not SSE".into()));
    }
    let mut decoder = Decoder::default();
    let ready_deadline = tokio::time::Instant::now() + READY_LIMIT;
    let mut ready = false;
    loop {
        let deadline = if ready {
            tokio::time::Instant::now() + SILENCE_LIMIT
        } else {
            ready_deadline
        };
        let chunk = tokio::time::timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| TransportError::TimedOut)?
            .map_err(TransportError::from_request)?
            .ok_or_else(|| TransportError::Stream("control stream closed".into()))?;
        for event in decoder.push(&chunk)? {
            if event == ControlEvent::Connected {
                ready = true;
                *failures = 0;
            }
            if sender.send(ControlItem::Event(event)).await.is_err() {
                return Ok(());
            }
        }
    }
}

#[derive(Debug, Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}

impl Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<ControlEvent>, TransportError> {
        let mut events = Vec::new();
        for byte in chunk {
            if self.after_cr && *byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = *byte == b'\r';
            if matches!(*byte, b'\n' | b'\r') {
                if let Some(event) = self.finish_line()? {
                    events.push(event);
                }
            } else {
                if self.line.len().saturating_add(self.data.len()) >= FRAME_LIMIT {
                    return Err(TransportError::Stream(
                        "control event exceeded its limit".into(),
                    ));
                }
                self.line.push(*byte);
            }
        }
        Ok(events)
    }

    fn finish_line(&mut self) -> Result<Option<ControlEvent>, TransportError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(None);
            }
            let data = std::mem::take(&mut self.data);
            return decode(&data);
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(data);
        }
        Ok(None)
    }
}

fn decode(data: &[u8]) -> Result<Option<ControlEvent>, TransportError> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        kind: String,
        properties: Option<serde_json::Value>,
    }
    let envelope: Envelope = serde_json::from_slice(data)
        .map_err(|_| TransportError::Stream("invalid control event".into()))?;
    match envelope.kind.as_str() {
        "openchamber:event-stream-ready" => Ok(Some(ControlEvent::Connected)),
        "openchamber:session-created" => Ok(Some(ControlEvent::SessionCreated)),
        "openchamber:terminal-sessions-changed" => Ok(Some(ControlEvent::TerminalSessionsChanged)),
        "openchamber:worktree-changed" => {
            #[derive(Deserialize)]
            struct Changed {
                directories: Vec<String>,
            }
            let Some(properties) = envelope.properties else {
                return Ok(None);
            };
            let Ok(changed) = serde_json::from_value::<Changed>(properties) else {
                return Ok(None);
            };
            if changed.directories.is_empty()
                || changed
                    .directories
                    .iter()
                    .any(|value| value.trim().is_empty())
            {
                return Ok(None);
            }
            Ok(Some(ControlEvent::WorktreeChanged {
                directories: changed.directories,
            }))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn control_subscription_uses_the_authenticated_sse_route_and_drop_closes_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("SSE request");
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0_u8; 1024];
                let count = socket.read(&mut bytes).await.expect("request headers");
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            let request = String::from_utf8(request)
                .expect("ASCII request")
                .to_ascii_lowercase();
            assert!(request.starts_with("get /prefix/api/openchamber/events http/1.1\r\n"));
            assert!(request.contains("authorization: bearer oc_client_synthetic"));
            assert!(request.contains("accept: text/event-stream"));
            assert!(!request.contains("oc_url_token"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"openchamber:event-stream-ready\",\"properties\":{}}\n\ndata: {\"type\":\"openchamber:worktree-changed\",\"properties\":{\"directories\":[\"/repo\",\"/linked\"]}}\n\n").await.expect("SSE events");
            let mut byte = [0_u8; 1];
            let count = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .expect("dropped subscription closes its request")
                .expect("EOF");
            assert_eq!(count, 0);
        });
        let endpoint = crate::endpoint::Endpoint::parse(&format!("http://{address}/prefix/"))
            .expect("endpoint");
        let client = ServerClient::new(
            endpoint,
            crate::credential::Credential::ClientToken("oc_client_synthetic".into()),
        )
        .expect("client");
        let mut stream = ControlStream::subscribe(client);
        assert!(matches!(
            stream.next().await,
            Some(ControlItem::Event(ControlEvent::Connected))
        ));
        assert!(
            matches!(stream.next().await, Some(ControlItem::Event(ControlEvent::WorktreeChanged { directories })) if directories == ["/repo", "/linked"])
        );
        drop(stream);
        server.await.expect("server");
    }

    #[test]
    fn control_topology_decodes_fragmented_crlf_and_multiline_data() {
        let bytes = b": heartbeat\r\ndata: {\"type\":\"openchamber:worktree-changed\",\r\ndata: \"properties\":{\"directories\":[\"/repo\",\"/linked\"]}}\r\n\r\n";
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(decoder.push(&[*byte]).expect("valid SSE"));
        }
        assert_eq!(
            events,
            [ControlEvent::WorktreeChanged {
                directories: vec!["/repo".into(), "/linked".into()]
            }]
        );
    }

    #[test]
    fn unknown_controls_do_not_invent_topology_and_frames_are_bounded() {
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .push(b"data: {\"type\":\"unrelated\"}\n\n")
                .expect("unknown control")
                .is_empty()
        );
        assert!(decoder.push(&vec![b'x'; FRAME_LIMIT + 1]).is_err());
    }
}
