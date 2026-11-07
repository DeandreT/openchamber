//! Dictation against the server's transcription (`lib/dictation`): the
//! status and model routes the Voice settings page uses, and the
//! `/api/dictation/ws` stream a recording sends its audio to.
//!
//! The protocol is `dictation-client.ts`'s: JSON text frames, a `start` the
//! server answers with `ack`, numbered `chunk`s of base64 16 kHz mono PCM16,
//! then `finish` with the last chunk's number and the `final` transcript.
//! The server transcribes on commit only; its `partial`s are the text of
//! segments it already committed, kept so a failed dictation can be saved.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

const SOCKET_ROUTE: &str = "api/dictation/ws";
/// The format every chunk is in.
pub const PCM_FORMAT: &str = "audio/pcm;rate=16000;bits=16";
/// `reasonCode` while the local model downloads; the stream start is retried.
pub const MODEL_DOWNLOADING: &str = "model_download_in_progress";

/// `CONNECT_TIMEOUT_MS`, `START_TIMEOUT_MS` and the finish timeouts.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub connect: Duration,
    pub start: Duration,
    pub finish: Duration,
    /// Added to the server's `finish_accepted.timeoutMs`.
    pub finish_margin: Duration,
    /// How often a start is retried while the model downloads.
    pub download_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            start: Duration::from_secs(30),
            finish: Duration::from_secs(60),
            finish_margin: Duration::from_secs(5),
            download_retry: Duration::from_secs(3),
        }
    }
}

/// `sttProvider`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SttProvider {
    #[default]
    Local,
    OpenAiCompatible,
}

impl SttProvider {
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }
}

/// `getDictationStartOptions`: what the user chose in Voice settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartOptions {
    pub provider: SttProvider,
    /// Trimmed; empty means detect.
    pub language: String,
    /// Fix misheard words with the Small Model.
    pub correct: bool,
    /// The local model; empty lets the server choose.
    pub local_model: String,
    pub server_url: String,
    pub model: String,
    pub endpoint_path: String,
}

impl StartOptions {
    fn to_json(&self) -> Value {
        let mut options = serde_json::Map::new();
        options.insert("provider".into(), json!(self.provider.as_setting()));
        let language = self.language.trim();
        if !language.is_empty() {
            options.insert("language".into(), json!(language));
        }
        if self.correct {
            options.insert("correct".into(), json!(true));
        }
        match self.provider {
            SttProvider::Local => {
                options.insert("localModel".into(), json!(self.local_model));
            }
            SttProvider::OpenAiCompatible => {
                let mut endpoint = serde_json::Map::new();
                endpoint.insert("baseUrl".into(), json!(self.server_url));
                endpoint.insert("model".into(), json!(self.model));
                if !self.endpoint_path.is_empty() {
                    endpoint.insert("endpointPath".into(), json!(self.endpoint_path));
                }
                options.insert("openaiCompatible".into(), Value::Object(endpoint));
            }
        }
        Value::Object(options)
    }
}

/// A failure the server or the connection reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationError {
    pub message: String,
    pub retryable: bool,
    pub reason_code: Option<String>,
}

impl DictationError {
    fn local(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            retryable: true,
            reason_code: None,
        }
    }

    pub fn model_downloading(&self) -> bool {
        self.reason_code.as_deref() == Some(MODEL_DOWNLOADING)
    }
}

/// What a stream reports to its recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// The server accepted the stream; buffered chunks are on their way.
    Started,
    /// The start failed; chunks stay buffered and the start is retried at
    /// finish (and every few seconds while the model downloads).
    StartFailed(DictationError),
    /// The text of the segments the server has committed so far.
    Partial(String),
    /// The transcript; the stream has ended.
    Final(String),
    /// The dictation failed; the stream has ended.
    Failed(DictationError),
}

enum Command {
    Chunk(String),
    Finish,
}

/// One dictation's connection. Dropping it cancels the dictation.
#[derive(Debug)]
pub struct DictationStream {
    commands: mpsc::UnboundedSender<Command>,
}

impl DictationStream {
    /// Starts a stream on the current Tokio runtime.
    pub fn open(
        client: ServerClient,
        options: StartOptions,
        dictation_id: String,
        timing: Timing,
    ) -> (Self, mpsc::UnboundedReceiver<StreamEvent>) {
        let (commands, received) = mpsc::unbounded_channel();
        let (events, reports) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut session = Session {
                client,
                options,
                dictation_id,
                timing,
                events,
                commands: received,
                chunks: Vec::new(),
                sent: 0,
                finishing: false,
            };
            let outcome = session.run().await;
            if let Some(outcome) = outcome {
                let _ = session.events.send(outcome);
            }
        });
        (Self { commands }, reports)
    }

    /// One base64 PCM16 chunk, in order.
    pub fn send_chunk(&self, audio: String) {
        let _ = self.commands.send(Command::Chunk(audio));
    }

    /// No more audio: the server transcribes what it has.
    pub fn finish(&self) {
        let _ = self.commands.send(Command::Finish);
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Session {
    client: ServerClient,
    options: StartOptions,
    dictation_id: String,
    timing: Timing,
    events: mpsc::UnboundedSender<StreamEvent>,
    commands: mpsc::UnboundedReceiver<Command>,
    /// Every chunk of the dictation, kept until it ends so a reconnect can
    /// send them again (`DictationStreamSender`).
    chunks: Vec<String>,
    sent: usize,
    /// The recording ended; no more chunks come.
    finishing: bool,
}

/// A message from the server about this dictation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServerMessage {
    Ready,
    Ack,
    Partial(String),
    FinishAccepted(Option<u64>),
    Final(String),
    Error(DictationError),
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    dictation_id: Option<String>,
    text: Option<String>,
    timeout_ms: Option<f64>,
    error: Option<String>,
    retryable: Option<bool>,
    reason_code: Option<String>,
}

/// Decodes a frame; a frame of another dictation is `Other`.
pub(crate) fn decode(text: &str, dictation_id: &str) -> ServerMessage {
    let Ok(frame) = serde_json::from_str::<Frame>(text) else {
        return ServerMessage::Other;
    };
    if frame.kind == "ready" {
        return ServerMessage::Ready;
    }
    if frame.dictation_id.as_deref() != Some(dictation_id) {
        return ServerMessage::Other;
    }
    match frame.kind.as_str() {
        "ack" => ServerMessage::Ack,
        "partial" => ServerMessage::Partial(frame.text.unwrap_or_default()),
        "finish_accepted" => ServerMessage::FinishAccepted(
            frame
                .timeout_ms
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .map(|ms| ms as u64),
        ),
        "final" => ServerMessage::Final(frame.text.unwrap_or_default()),
        "error" => ServerMessage::Error(DictationError {
            message: frame
                .error
                .filter(|error| !error.is_empty())
                .unwrap_or_else(|| "Dictation failed".to_owned()),
            retryable: frame.retryable != Some(false),
            reason_code: frame.reason_code,
        }),
        _ => ServerMessage::Other,
    }
}

enum Started {
    Yes(Socket),
    /// The start failed; the error is the user's to see.
    No(DictationError),
    /// The recording went away.
    Gone,
}

impl Session {
    async fn send(socket: &mut Socket, message: Value) -> bool {
        socket
            .send(WsMessage::Text(message.to_string().into()))
            .await
            .is_ok()
    }

    async fn connect(&self) -> Result<Socket, DictationError> {
        let failed = || DictationError::local("Dictation connection failed");
        let url = self.client.endpoint().websocket_url(SOCKET_ROUTE);
        let mut request = url.as_str().into_client_request().map_err(|_| failed())?;
        request.headers_mut().insert(
            "origin",
            HeaderValue::from_str(
                &self
                    .client
                    .endpoint()
                    .url("")
                    .origin()
                    .ascii_serialization(),
            )
            .map_err(|_| failed())?,
        );
        if let Some(value) = self.client.credential().authorization() {
            let mut value = HeaderValue::from_str(&value).map_err(|_| failed())?;
            value.set_sensitive(true);
            request.headers_mut().insert("authorization", value);
        }
        let (mut socket, _) = tokio::time::timeout(
            self.timing.connect,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| DictationError::local("Dictation connection timed out"))?
        .map_err(|_| failed())?;
        // The server says `ready` before it reads anything.
        let ready = tokio::time::timeout(self.timing.connect, async {
            while let Some(Ok(message)) = socket.next().await {
                if let WsMessage::Text(text) = message
                    && decode(&text, &self.dictation_id) == ServerMessage::Ready
                {
                    return true;
                }
            }
            false
        })
        .await;
        match ready {
            Ok(true) => Ok(socket),
            Ok(false) => Err(failed()),
            Err(_) => Err(DictationError::local("Dictation connection timed out")),
        }
    }

    /// Connects and starts the stream, buffering chunks meanwhile.
    async fn start(&mut self) -> Started {
        let mut socket = match self.connect().await {
            Ok(socket) => socket,
            Err(error) => return Started::No(error),
        };
        let start = json!({
            "type": "start",
            "dictationId": self.dictation_id,
            "format": PCM_FORMAT,
            "options": self.options.to_json(),
        });
        if !Self::send(&mut socket, start).await {
            return Started::No(DictationError::local("Dictation connection lost"));
        }
        let deadline = tokio::time::sleep(self.timing.start);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => {
                    return Started::No(DictationError::local("Dictation start timed out"));
                }
                command = self.commands.recv() => match command {
                    Some(Command::Chunk(chunk)) => self.chunks.push(chunk),
                    // Finishing waits for the start like every other chunk.
                    Some(Command::Finish) => {
                        return self.started_after_finish(socket, &mut deadline).await;
                    }
                    None => {
                        Self::cancel(&mut socket, &self.dictation_id).await;
                        return Started::Gone;
                    }
                },
                message = socket.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => match decode(&text, &self.dictation_id) {
                        ServerMessage::Ack => return Started::Yes(socket),
                        ServerMessage::Error(error) => return Started::No(error),
                        _ => {}
                    },
                    Some(Ok(_)) => {}
                    _ => return Started::No(DictationError::local("Dictation connection lost")),
                },
            }
        }
    }

    /// The recording finished before the server acknowledged the start.
    async fn started_after_finish(
        &mut self,
        mut socket: Socket,
        deadline: &mut std::pin::Pin<&mut tokio::time::Sleep>,
    ) -> Started {
        self.finishing = true;
        loop {
            tokio::select! {
                () = &mut *deadline => {
                    return Started::No(DictationError::local("Dictation start timed out"));
                }
                message = socket.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => match decode(&text, &self.dictation_id) {
                        ServerMessage::Ack => return Started::Yes(socket),
                        ServerMessage::Error(error) => return Started::No(error),
                        _ => {}
                    },
                    Some(Ok(_)) => {}
                    _ => return Started::No(DictationError::local("Dictation connection lost")),
                },
            }
        }
    }

    async fn cancel(socket: &mut Socket, dictation_id: &str) {
        let _ = Self::send(
            socket,
            json!({ "type": "cancel", "dictationId": dictation_id }),
        )
        .await;
        let _ = socket.close(None).await;
    }

    async fn flush(&mut self, socket: &mut Socket) -> bool {
        while self.sent < self.chunks.len() {
            let chunk = json!({
                "type": "chunk",
                "dictationId": self.dictation_id,
                "seq": self.sent,
                "audio": self.chunks[self.sent],
            });
            if !Self::send(socket, chunk).await {
                return false;
            }
            self.sent += 1;
        }
        true
    }

    /// Runs the dictation; the returned event ends it.
    async fn run(&mut self) -> Option<StreamEvent> {
        let mut socket = loop {
            match self.start().await {
                Started::Yes(socket) => break socket,
                Started::Gone => return None,
                Started::No(error) => {
                    if self.finishing {
                        return Some(StreamEvent::Failed(error));
                    }
                    let downloading = error.model_downloading();
                    let _ = self.events.send(StreamEvent::StartFailed(error));
                    // Keep buffering until finish, or retry while the model
                    // downloads; finish starts once more.
                    if !self.wait_to_retry(downloading).await {
                        return None;
                    }
                }
            }
        };
        let _ = self.events.send(StreamEvent::Started);
        self.sent = 0;
        if !self.flush(&mut socket).await {
            return Some(self.lost());
        }
        let mut finish_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        if self.finishing && !self.send_finish(&mut socket, &mut finish_deadline).await {
            return Some(self.lost());
        }
        loop {
            let timeout = async {
                match finish_deadline.as_mut() {
                    Some(deadline) => deadline.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = timeout => {
                    Self::cancel(&mut socket, &self.dictation_id).await;
                    return Some(StreamEvent::Failed(DictationError::local(
                        "Timed out waiting for transcription",
                    )));
                }
                command = self.commands.recv(), if !self.finishing => match command {
                    Some(Command::Chunk(chunk)) => {
                        self.chunks.push(chunk);
                        if !self.flush(&mut socket).await {
                            return Some(self.lost());
                        }
                    }
                    Some(Command::Finish) => {
                        self.finishing = true;
                        if !self.send_finish(&mut socket, &mut finish_deadline).await {
                            return Some(self.lost());
                        }
                    }
                    None => {
                        Self::cancel(&mut socket, &self.dictation_id).await;
                        return None;
                    }
                },
                message = socket.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => match decode(&text, &self.dictation_id) {
                        ServerMessage::Partial(text) => {
                            let _ = self.events.send(StreamEvent::Partial(text));
                        }
                        ServerMessage::FinishAccepted(Some(ms)) if self.finishing => {
                            finish_deadline = Some(Box::pin(tokio::time::sleep(
                                Duration::from_millis(ms) + self.timing.finish_margin,
                            )));
                        }
                        ServerMessage::Final(text) => {
                            let _ = socket.close(None).await;
                            return Some(StreamEvent::Final(text));
                        }
                        ServerMessage::Error(error) => {
                            let _ = socket.close(None).await;
                            return Some(StreamEvent::Failed(error));
                        }
                        _ => {}
                    },
                    Some(Ok(_)) => {}
                    _ => return Some(self.lost()),
                },
            }
        }
    }

    fn lost(&self) -> StreamEvent {
        StreamEvent::Failed(DictationError::local("Dictation connection lost"))
    }

    async fn send_finish(
        &mut self,
        socket: &mut Socket,
        deadline: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    ) -> bool {
        *deadline = Some(Box::pin(tokio::time::sleep(self.timing.finish)));
        let finish = json!({
            "type": "finish",
            "dictationId": self.dictation_id,
            "finalSeq": self.chunks.len() as i64 - 1,
        });
        Self::send(socket, finish).await
    }

    /// Buffers chunks until finish or, while the model downloads, the next
    /// retry. `false` when the recording went away.
    async fn wait_to_retry(&mut self, downloading: bool) -> bool {
        let retry = async {
            if downloading {
                tokio::time::sleep(self.timing.download_retry).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(retry);
        loop {
            tokio::select! {
                () = &mut retry => return true,
                command = self.commands.recv() => match command {
                    Some(Command::Chunk(chunk)) => self.chunks.push(chunk),
                    Some(Command::Finish) => {
                        self.finishing = true;
                        return true;
                    }
                    None => return false,
                },
            }
        }
    }
}

/// A local speech model as the status route lists it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModel {
    pub id: String,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub downloading: bool,
    pub download_progress: Option<f64>,
    pub download_error: Option<String>,
}

/// `GET /api/dictation/status`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationStatus {
    /// The model the server dictates with: the saved one, or its choice.
    pub active_model: Option<String>,
    #[serde(default)]
    pub models: Vec<LocalModel>,
}

/// `GET /api/voice/keys`: which voice API keys the server holds. The keys
/// themselves never come back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct VoiceKeys {
    pub openai: bool,
    #[serde(rename = "openaiCompatible")]
    pub openai_compatible: bool,
    pub stt: bool,
}

impl ServerClient {
    pub async fn voice_keys(&self) -> Result<VoiceKeys, TransportError> {
        self.get_json(self.endpoint().url("api/voice/keys"), None)
            .await
    }

    /// Saves the transcription key, or removes it with `None`.
    pub async fn set_voice_stt_key(&self, key: Option<&str>) -> Result<VoiceKeys, TransportError> {
        let response = self
            .authorized(Method::PUT, self.endpoint().url("api/voice/keys"))
            .json(&json!({ "stt": key }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        read_json(response).await
    }

    /// The local models and the one the server would dictate with.
    pub async fn dictation_status(
        &self,
        local_model: &str,
    ) -> Result<DictationStatus, TransportError> {
        let mut url = self.endpoint().url("api/dictation/status");
        url.query_pairs_mut()
            .append_pair("provider", "local")
            .append_pair("localModel", local_model);
        self.get_json(url, None).await
    }

    /// Starts downloading a local model, or deletes it.
    pub async fn change_dictation_model(
        &self,
        model: &str,
        download: bool,
    ) -> Result<(), TransportError> {
        let mut url = self.endpoint().url("api/dictation/models/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments.pop_if_empty().push(model);
            if download {
                segments.push("download");
            }
        }
        let method = if download {
            Method::POST
        } else {
            Method::DELETE
        };
        let response = self
            .authorized(method, url)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        read_json::<Value>(response).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_options_match_the_web_client() {
        let local = StartOptions {
            language: " uk ".into(),
            ..StartOptions::default()
        };
        assert_eq!(
            local.to_json(),
            json!({"provider": "local", "language": "uk", "localModel": ""})
        );
        let remote = StartOptions {
            provider: SttProvider::OpenAiCompatible,
            correct: true,
            server_url: "http://localhost:8001/v1".into(),
            model: "whisper".into(),
            ..StartOptions::default()
        };
        assert_eq!(
            remote.to_json(),
            json!({
                "provider": "openai-compatible",
                "correct": true,
                "openaiCompatible": {"baseUrl": "http://localhost:8001/v1", "model": "whisper"},
            })
        );
    }

    #[test]
    fn frames_of_other_dictations_are_ignored() {
        assert_eq!(decode(r#"{"type":"ready"}"#, "d1"), ServerMessage::Ready);
        assert_eq!(
            decode(r#"{"type":"ack","dictationId":"d2","ackSeq":0}"#, "d1"),
            ServerMessage::Other
        );
        assert_eq!(
            decode(r#"{"type":"final","dictationId":"d1","text":"hi"}"#, "d1"),
            ServerMessage::Final("hi".into())
        );
        assert_eq!(
            decode(
                r#"{"type":"error","dictationId":"d1","error":"","retryable":false,"reasonCode":"model_download_in_progress"}"#,
                "d1"
            ),
            ServerMessage::Error(DictationError {
                message: "Dictation failed".into(),
                retryable: false,
                reason_code: Some(MODEL_DOWNLOADING.into()),
            })
        );
        assert_eq!(
            decode(
                r#"{"type":"finish_accepted","dictationId":"d1","timeoutMs":1500}"#,
                "d1"
            ),
            ServerMessage::FinishAccepted(Some(1500))
        );
        assert_eq!(decode("not json", "d1"), ServerMessage::Other);
    }
}
