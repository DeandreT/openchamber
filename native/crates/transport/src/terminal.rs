//! Server terminals: the HTTP command plane and the protocol v3 socket.
//!
//! The server owns every PTY (`packages/web/server/lib/terminal/runtime.js`).
//! [`TerminalApi`] creates, lists, resizes, restarts and closes them;
//! [`TerminalSocket`] carries their output and input over one WebSocket.
//! Every frame in both directions is binary: the tag byte `0x01` followed by
//! UTF-8 JSON. Dropping a socket detaches its terminals and never stops one.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::client::{ServerClient, encode_directory, read_json};
use crate::error::TransportError;

/// The first byte of every control frame (`TERMINAL_WS_CONTROL_TAG_JSON`).
pub const FRAME_TAG: u8 = 0x01;
/// The server's WebSocket `maxPayload` (`TERMINAL_WS_MAX_PAYLOAD_BYTES`).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// The server rejects a `write` whose JavaScript string is longer
/// (`MAX_INPUT_CHARS`, counted in UTF-16 code units).
pub const MAX_WRITE_UNITS: usize = 65_536;
/// The code a create or restart answers for a deleted working directory.
pub const CWD_MISSING_CODE: &str = "TERMINAL_CWD_MISSING";
const PROTOCOL: u8 = 3;
const SOCKET_ROUTE: &str = "api/terminal/ws";
const CHANNEL_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// HTTP command plane
// ---------------------------------------------------------------------------

/// A shell the server can start (`GET /api/terminal/shells`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalShell {
    pub id: String,
    pub name: String,
    pub supports_login: bool,
}

/// Whether the terminal's process still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Running,
    Exited,
}

/// What a terminal was started for. Project actions keep their run IDs so a
/// rerun can replace them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalPurpose {
    Terminal,
    ProjectAction {
        action_id: String,
        execution_id: String,
    },
}

/// A terminal the server holds (`GET /api/terminal/sessions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerTerminal {
    pub session_id: String,
    pub cwd: String,
    pub status: TerminalStatus,
    pub created_at: Option<i64>,
    /// `false` for a command-mode terminal, which cannot be restarted.
    pub interactive: bool,
    pub purpose: TerminalPurpose,
}

/// The light or dark appearance a shell's color queries are answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// What a new interactive terminal starts with (`POST /api/terminal/create`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateTerminal {
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub theme_mode: ThemeMode,
    /// CSS colors the server answers OSC 10/11 queries with.
    pub background: Option<String>,
    pub foreground: Option<String>,
    /// `auto` or a shell ID from [`TerminalApi::shells`].
    pub shell: String,
    pub login_shell: bool,
}

/// A started terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedTerminal {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
    pub status: TerminalStatus,
    /// `false` when the server started a shell for a command request, as a
    /// server without command mode does.
    pub command_mode: bool,
    pub purpose: Option<TerminalPurpose>,
}

/// A project action's run (`createProjectActionTerminalSession`): the
/// command runs in place of a shell, under the execution ID, which the
/// server also uses as the terminal's ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionCommand {
    pub command: String,
    pub action_id: String,
    pub execution_id: String,
}

/// The terminal settings of the desktop surface, each with the web
/// client's default when the server holds no valid value.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSettings {
    pub close_on_exit: bool,
    pub font_size: f32,
    pub shell: String,
    /// Shell IDs started as login shells.
    pub login_shells: Vec<String>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            close_on_exit: true,
            font_size: 14.0,
            shell: "auto".to_owned(),
            login_shells: Vec::new(),
        }
    }
}

/// Why a terminal command failed.
#[derive(Debug, thiserror::Error)]
pub enum TerminalRequestError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The server refused the command and said why. `code` is set for the
    /// cases a client can act on, such as [`CWD_MISSING_CODE`].
    #[error("the server refused the terminal command: {message}")]
    Refused {
        status: u16,
        message: String,
        code: Option<String>,
    },
}

impl TerminalRequestError {
    /// The working directory no longer exists, as after a deleted worktree.
    pub fn is_cwd_missing(&self) -> bool {
        matches!(self, Self::Refused { code: Some(code), .. } if code == CWD_MISSING_CODE)
    }
}

/// Terminal commands against one server.
#[derive(Clone, Debug)]
pub struct TerminalApi {
    client: ServerClient,
}

#[derive(Deserialize)]
struct ShellWire {
    id: String,
    name: String,
    #[serde(rename = "supportsLogin")]
    supports_login: bool,
}

#[derive(Deserialize)]
struct SessionListWire {
    sessions: Vec<Value>,
}

#[derive(Deserialize)]
struct ServerTerminalWire {
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
    status: String,
    #[serde(rename = "createdAt")]
    created_at: Option<i64>,
    mode: Option<String>,
    purpose: Option<Value>,
}

#[derive(Deserialize)]
struct CreatedWire {
    #[serde(rename = "sessionId")]
    session_id: String,
    cols: u16,
    rows: u16,
    status: String,
    mode: Option<String>,
    purpose: Option<Value>,
}

#[derive(Deserialize)]
struct ErrorWire {
    error: Option<String>,
    code: Option<String>,
}

impl TerminalApi {
    pub fn new(client: ServerClient) -> Self {
        Self { client }
    }

    /// The server's shells.
    pub async fn shells(&self) -> Result<Vec<TerminalShell>, TerminalRequestError> {
        let url = self.client.endpoint().url("api/terminal/shells");
        let response = self.send(Method::GET, url, None, None).await?;
        let shells: Vec<ShellWire> = read_json(response).await?;
        Ok(shells
            .into_iter()
            .map(|shell| TerminalShell {
                id: shell.id,
                name: shell.name,
                supports_login: shell.supports_login,
            })
            .collect())
    }

    /// The terminals the server holds for `cwd`. A malformed entry is left
    /// out; a malformed list is an error, never an empty success.
    pub async fn sessions(&self, cwd: &str) -> Result<Vec<ServerTerminal>, TerminalRequestError> {
        let mut url = self.client.endpoint().url("api/terminal/sessions");
        url.query_pairs_mut().append_pair("cwd", cwd);
        let response = self.send(Method::GET, url, None, Some(cwd)).await?;
        let list: SessionListWire = read_json(response).await?;
        Ok(list
            .sessions
            .into_iter()
            .filter_map(|entry| serde_json::from_value::<ServerTerminalWire>(entry).ok())
            .filter_map(|wire| {
                let status = match wire.status.as_str() {
                    "running" => TerminalStatus::Running,
                    "exited" => TerminalStatus::Exited,
                    _ => return None,
                };
                Some(ServerTerminal {
                    session_id: wire.session_id,
                    cwd: wire.cwd,
                    status,
                    created_at: wire.created_at,
                    interactive: wire.mode.as_deref() != Some("command"),
                    purpose: parse_purpose(wire.purpose.as_ref())?,
                })
            })
            .collect())
    }

    /// Keeps the idle sweep from reaping these terminals for another 30
    /// minutes. Answers how many the server knew.
    pub async fn touch(
        &self,
        session_ids: &[String],
        directory: &str,
    ) -> Result<u64, TerminalRequestError> {
        let url = self.client.endpoint().url("api/terminal/touch");
        let body = json!({ "sessionIds": session_ids });
        let response = self
            .send(Method::POST, url, Some(body), Some(directory))
            .await?;
        let value: Value = read_json(response).await?;
        Ok(value.get("touched").and_then(Value::as_u64).unwrap_or(0))
    }

    /// Starts an interactive terminal.
    pub async fn create(
        &self,
        request: &CreateTerminal,
    ) -> Result<CreatedTerminal, TerminalRequestError> {
        let url = self.client.endpoint().url("api/terminal/create");
        let response = self
            .send(
                Method::POST,
                url,
                Some(create_body(request)),
                Some(&request.cwd),
            )
            .await?;
        created(response).await
    }

    /// Starts a project action's command. A running terminal of the same
    /// action in `cwd` answers instead, as the server joins duplicate runs.
    pub async fn create_command(
        &self,
        request: &CreateTerminal,
        command: &ActionCommand,
    ) -> Result<CreatedTerminal, TerminalRequestError> {
        let url = self.client.endpoint().url("api/terminal/create");
        let mut body = create_body(request);
        body["sessionId"] = json!(command.execution_id);
        body["mode"] = json!("command");
        body["command"] = json!(command.command);
        body["purpose"] = json!({
            "type": "project-action",
            "actionId": command.action_id,
            "executionId": command.execution_id,
        });
        let response = self
            .send(Method::POST, url, Some(body), Some(&request.cwd))
            .await?;
        created(response).await
    }

    /// Stops the terminal's process at once and forgets it (SIGKILL).
    pub async fn force_kill(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Result<(), TerminalRequestError> {
        let url = self.client.endpoint().url("api/terminal/force-kill");
        let body = json!({ "sessionId": session_id });
        self.send(Method::POST, url, Some(body), Some(directory))
            .await?;
        Ok(())
    }

    /// Starts a new shell in place of the terminal's process, keeping its ID.
    pub async fn restart(
        &self,
        session_id: &str,
        request: &CreateTerminal,
    ) -> Result<CreatedTerminal, TerminalRequestError> {
        let url = self.session_url(session_id, "restart");
        let response = self
            .send(
                Method::POST,
                url,
                Some(create_body(request)),
                Some(&request.cwd),
            )
            .await?;
        created(response).await
    }

    /// Sets the PTY size. The server accepts 1 to 1000 columns and 1 to 500 rows.
    pub async fn resize(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
        directory: &str,
    ) -> Result<(), TerminalRequestError> {
        let url = self.session_url(session_id, "resize");
        let body = json!({ "cols": cols, "rows": rows });
        self.send(Method::POST, url, Some(body), Some(directory))
            .await?;
        Ok(())
    }

    /// Stops the terminal's process and forgets it: SIGTERM, then SIGKILL
    /// after a second.
    pub async fn close(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Result<(), TerminalRequestError> {
        let mut url = self.client.endpoint().url("api/terminal");
        url.path_segments_mut()
            .expect("an http URL has path segments")
            .push(session_id);
        self.send(Method::DELETE, url, None, Some(directory))
            .await?;
        Ok(())
    }

    /// The desktop surface's terminal settings.
    pub async fn settings(&self) -> Result<TerminalSettings, TerminalRequestError> {
        let response = self
            .send(Method::GET, self.client.settings_url(), None, None)
            .await?;
        let value: Value = read_json(response).await?;
        Ok(parse_settings(&value))
    }

    fn session_url(&self, session_id: &str, action: &str) -> Url {
        let mut url = self.client.endpoint().url("api/terminal");
        url.path_segments_mut()
            .expect("an http URL has path segments")
            .push(session_id)
            .push(action);
        url
    }

    /// Sends one command and maps a refusal to its message and code.
    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        directory: Option<&str>,
    ) -> Result<Response, TerminalRequestError> {
        let mut request = self.client.authorized(method, url);
        if let Some(directory) = directory {
            request = request.header("x-opencode-directory", encode_directory(directory));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TransportError::Redirected.into());
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(TransportError::AuthenticationRequired.into());
        }
        if status.is_client_error() {
            let body: Option<ErrorWire> = response.json().await.ok();
            let (message, code) = body.map(|body| (body.error, body.code)).unwrap_or_default();
            return Err(TerminalRequestError::Refused {
                status: status.as_u16(),
                message: message.unwrap_or_else(|| format!("HTTP {}", status.as_u16())),
                code,
            });
        }
        if !status.is_success() {
            return Err(TransportError::Http {
                status: status.as_u16(),
            }
            .into());
        }
        Ok(response)
    }
}

fn create_body(request: &CreateTerminal) -> Value {
    let mut body = json!({
        "cwd": request.cwd,
        "cols": request.cols,
        "rows": request.rows,
        "themeMode": match request.theme_mode {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        },
        "shell": request.shell,
        "loginShell": request.login_shell,
    });
    if let Some(background) = &request.background {
        body["terminalBackground"] = json!(background);
    }
    if let Some(foreground) = &request.foreground {
        body["terminalForeground"] = json!(foreground);
    }
    body
}

async fn created(response: Response) -> Result<CreatedTerminal, TerminalRequestError> {
    let wire: CreatedWire = read_json(response).await?;
    let status = match wire.status.as_str() {
        "running" => TerminalStatus::Running,
        "exited" => TerminalStatus::Exited,
        other => {
            return Err(TransportError::Malformed(format!("terminal status {other}")).into());
        }
    };
    Ok(CreatedTerminal {
        session_id: wire.session_id,
        cols: wire.cols,
        rows: wire.rows,
        status,
        command_mode: wire.mode.as_deref() == Some("command"),
        purpose: parse_purpose(wire.purpose.as_ref()),
    })
}

fn parse_purpose(value: Option<&Value>) -> Option<TerminalPurpose> {
    let Some(value) = value else {
        return Some(TerminalPurpose::Terminal);
    };
    match value.get("type")?.as_str()? {
        "terminal" => Some(TerminalPurpose::Terminal),
        "project-action" => Some(TerminalPurpose::ProjectAction {
            action_id: value.get("actionId")?.as_str()?.to_owned(),
            execution_id: value.get("executionId")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

/// Reads each setting on its own; a malformed value keeps the default.
fn parse_settings(value: &Value) -> TerminalSettings {
    let mut settings = TerminalSettings::default();
    if let Some(close) = value.get("terminalCloseOnExit").and_then(Value::as_bool) {
        settings.close_on_exit = close;
    }
    if let Some(size) = value.get("terminalFontSize").and_then(Value::as_f64)
        && size.is_finite()
        && size > 0.0
    {
        // `setTerminalFontSize` clamps the same way.
        settings.font_size = (size as f32).clamp(9.0, 32.0);
    }
    if let Some(shell) = value.get("terminalShell").and_then(Value::as_str)
        && !shell.trim().is_empty()
    {
        settings.shell = shell.to_owned();
    }
    if let Some(shells) = value.get("terminalLoginShells").and_then(Value::as_array) {
        settings.login_shells = shells
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    settings
}

// ---------------------------------------------------------------------------
// Protocol v3 codec
// ---------------------------------------------------------------------------

/// A frame the client sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientFrame<'a> {
    Hello,
    Ping,
    Attach(&'a str),
    Detach(&'a str),
    Write { session: &'a str, data: &'a str },
}

/// Encodes a client frame: the tag byte, then its JSON.
pub fn encode(frame: &ClientFrame<'_>) -> Vec<u8> {
    let value = match frame {
        ClientFrame::Hello => json!({ "t": "hello", "v": PROTOCOL }),
        ClientFrame::Ping => json!({ "t": "ping", "v": PROTOCOL }),
        ClientFrame::Attach(session) => json!({ "t": "attach", "v": PROTOCOL, "s": session }),
        ClientFrame::Detach(session) => json!({ "t": "detach", "v": PROTOCOL, "s": session }),
        ClientFrame::Write { session, data } => {
            json!({ "t": "write", "v": PROTOCOL, "s": session, "d": data })
        }
    };
    let mut bytes = Vec::with_capacity(64);
    bytes.push(FRAME_TAG);
    serde_json::to_writer(&mut bytes, &value).expect("a JSON value always serializes");
    bytes
}

/// What a snapshot carries: the server's whole retained history at the PTY
/// size it was drawn for.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub session: String,
    pub sequence: u64,
    pub history: String,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub status: TerminalStatus,
    pub exit_code: Option<i64>,
    pub signal: Option<i64>,
}

/// A frame the server sends.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerFrame {
    Hello,
    Pong,
    Snapshot(Snapshot),
    /// Raw PTY output. `replay` is the history-safe form, sent only when it
    /// differs; a live emulator writes `data`.
    Output {
        session: String,
        sequence: u64,
        data: String,
        replay: Option<String>,
    },
    Exit {
        session: String,
        sequence: u64,
        exit_code: Option<i64>,
        signal: Option<i64>,
    },
    /// A new process replaced the old one; `history` restarts the screen.
    Restarted {
        session: String,
        sequence: u64,
        history: String,
    },
    Error {
        session: Option<String>,
        code: Option<String>,
        message: Option<String>,
        fatal: bool,
    },
}

impl ServerFrame {
    fn sequenced(&self) -> Option<(&str, u64)> {
        match self {
            Self::Output {
                session, sequence, ..
            }
            | Self::Exit {
                session, sequence, ..
            }
            | Self::Restarted {
                session, sequence, ..
            } => Some((session, *sequence)),
            Self::Snapshot(snapshot) => Some((&snapshot.session, snapshot.sequence)),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct FrameWire {
    t: String,
    s: Option<String>,
    q: Option<u64>,
    d: Option<String>,
    r: Option<String>,
    history: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    status: Option<String>,
    #[serde(rename = "exitCode")]
    exit_code: Option<i64>,
    signal: Option<i64>,
    code: Option<String>,
    message: Option<String>,
    fatal: Option<bool>,
}

/// Decodes a server frame. `None` for anything the client does not accept,
/// as `terminalMessageSchema` drops it: a missing tag, malformed JSON or a
/// frame without the fields its type requires.
pub fn decode(bytes: &[u8]) -> Option<ServerFrame> {
    let payload = match bytes.first() {
        Some(&FRAME_TAG) => &bytes[1..],
        _ => bytes,
    };
    let wire: FrameWire = serde_json::from_slice(payload).ok()?;
    let status = |value: Option<&str>| match value {
        Some("running") => Some(TerminalStatus::Running),
        Some("exited") | Some("error") => Some(TerminalStatus::Exited),
        _ => None,
    };
    Some(match wire.t.as_str() {
        "hello" => ServerFrame::Hello,
        "pong" => ServerFrame::Pong,
        "error" => ServerFrame::Error {
            session: wire.s,
            code: wire.code,
            message: wire.message,
            fatal: wire.fatal == Some(true),
        },
        "snapshot" => ServerFrame::Snapshot(Snapshot {
            session: wire.s?,
            sequence: wire.q.unwrap_or(0),
            history: wire.history.unwrap_or_default(),
            cols: wire.cols.filter(|cols| *cols > 0),
            rows: wire.rows.filter(|rows| *rows > 0),
            status: status(wire.status.as_deref())?,
            exit_code: wire.exit_code,
            signal: wire.signal,
        }),
        "output" => ServerFrame::Output {
            session: wire.s?,
            sequence: wire.q?,
            data: wire.d?,
            replay: wire.r,
        },
        "exit" => ServerFrame::Exit {
            session: wire.s?,
            sequence: wire.q?,
            exit_code: wire.exit_code,
            signal: wire.signal,
        },
        "restarted" => ServerFrame::Restarted {
            session: wire.s?,
            sequence: wire.q?,
            history: wire.history.unwrap_or_default(),
        },
        _ => return None,
    })
}

/// Splits input into `write` payloads the server accepts, in order.
///
/// Each frame stays within [`MAX_FRAME_BYTES`] after JSON escaping, and each
/// payload within [`MAX_WRITE_UNITS`] UTF-16 code units. Splits fall on
/// character boundaries, so the PTY receives the exact bytes in sequence.
pub fn split_write<'a>(session: &str, data: &'a str) -> Vec<&'a str> {
    let overhead = encode(&ClientFrame::Write { session, data: "" }).len();
    let budget = MAX_FRAME_BYTES.saturating_sub(overhead).max(6);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    let mut units = 0;
    for (index, character) in data.char_indices() {
        let escaped = escaped_len(character);
        let width = character.len_utf16();
        if bytes + escaped > budget || units + width > MAX_WRITE_UNITS {
            chunks.push(&data[start..index]);
            start = index;
            bytes = 0;
            units = 0;
        }
        bytes += escaped;
        units += width;
    }
    if start < data.len() {
        chunks.push(&data[start..]);
    }
    chunks
}

/// The bytes `serde_json` writes for a character inside a string.
fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        '\u{00}'..='\u{1f}' => 6,
        other => other.len_utf8(),
    }
}

// ---------------------------------------------------------------------------
// Reconnecting socket
// ---------------------------------------------------------------------------

/// The socket's timers. [`SocketTiming::default`] is the reference
/// client's contract (`terminalApi.ts`); tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketTiming {
    /// A socket that does not open this soon is a failed attempt.
    pub open_timeout: Duration,
    /// The first reconnect delay, doubled per consecutive failure.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// The wait after an authentication refusal, which needs new credentials.
    pub refused_delay: Duration,
    /// How often the client pings an open socket.
    pub keepalive: Duration,
    /// How long a socket with no attached terminal stays open for reuse.
    pub idle_grace: Duration,
}

impl Default for SocketTiming {
    fn default() -> Self {
        Self {
            open_timeout: Duration::from_secs(10),
            backoff_base: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(8),
            refused_delay: Duration::from_secs(60),
            keepalive: Duration::from_secs(45),
            idle_grace: Duration::from_secs(15),
        }
    }
}

impl SocketTiming {
    /// `min(base * 2^(failures - 1), cap)`.
    pub fn backoff(&self, failures: u32) -> Duration {
        let exponent = failures.saturating_sub(1).min(10);
        self.backoff_base
            .saturating_mul(1 << exponent)
            .min(self.backoff_cap)
    }
}

/// What the socket reports. Sequenced events arrive at most once and in
/// order per terminal: anything at or below the last delivered sequence is
/// dropped, and a snapshot resets that sequence.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalEvent {
    /// The socket opened and every attached terminal was asked for a fresh
    /// snapshot.
    Connected,
    /// The socket failed; the next attempt follows after `delay`.
    Reconnecting {
        attempt: u32,
        delay: Duration,
        /// The server refused the credentials; waiting longer will not help
        /// until they change.
        refused: bool,
    },
    Snapshot(Snapshot),
    Output {
        session: String,
        sequence: u64,
        data: String,
    },
    Exit {
        session: String,
        exit_code: Option<i64>,
        signal: Option<i64>,
    },
    Restarted {
        session: String,
        history: String,
    },
    /// The server reported a problem. A fatal one ended the attachment.
    Error {
        session: Option<String>,
        code: Option<String>,
        fatal: bool,
    },
    /// Input that could not be delivered, because the socket failed before
    /// it was sent.
    WriteFailed {
        session: String,
    },
}

#[derive(Debug)]
enum Command {
    Attach(String),
    Detach(String),
    Write { session: String, data: String },
}

/// The terminal socket of one server. Every attached terminal shares it.
#[derive(Debug)]
pub struct TerminalSocket {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::Receiver<TerminalEvent>,
    task: JoinHandle<()>,
}

impl TerminalSocket {
    /// Starts the socket on the current Tokio runtime. It dials on the first
    /// attach.
    pub fn start(client: ServerClient) -> Self {
        Self::start_with(client, SocketTiming::default())
    }

    pub fn start_with(client: ServerClient, timing: SocketTiming) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (sender, events) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(run(client, timing, receiver, sender));
        Self {
            commands,
            events,
            task,
        }
    }

    /// A handle that sends commands from any thread.
    pub fn commands(&self) -> TerminalCommands {
        TerminalCommands(self.commands.clone())
    }

    /// The next event, or `None` once the socket task ended.
    pub async fn next(&mut self) -> Option<TerminalEvent> {
        self.events.recv().await
    }
}

impl Drop for TerminalSocket {
    // Closing the socket detaches; the server keeps every PTY.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Sends commands to a [`TerminalSocket`].
#[derive(Clone, Debug)]
pub struct TerminalCommands(mpsc::UnboundedSender<Command>);

impl TerminalCommands {
    /// Receives the terminal's output, starting with a snapshot.
    pub fn attach(&self, session: &str) {
        let _ = self.0.send(Command::Attach(session.to_owned()));
    }

    /// Stops receiving the terminal's output. The terminal keeps running.
    pub fn detach(&self, session: &str) {
        let _ = self.0.send(Command::Detach(session.to_owned()));
    }

    /// Sends input to the terminal, split into frames the server accepts.
    pub fn write(&self, session: &str, data: String) {
        if data.is_empty() {
            return;
        }
        let _ = self.0.send(Command::Write {
            session: session.to_owned(),
            data,
        });
    }
}

/// The per-terminal state the socket keeps.
#[derive(Debug, Default)]
struct Attachments {
    /// The last delivered sequence per attached terminal; `None` until its
    /// first snapshot.
    last: BTreeMap<String, Option<u64>>,
}

impl Attachments {
    /// Whether `frame` is new for an attached terminal, recording it.
    fn accept(&mut self, session: &str, sequence: u64, snapshot: bool) -> bool {
        let Some(last) = self.last.get_mut(session) else {
            return false;
        };
        if snapshot {
            *last = Some(sequence);
            return true;
        }
        match *last {
            Some(previous) if sequence <= previous => false,
            _ => {
                *last = Some(sequence);
                true
            }
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Why a connection ended.
enum Ended {
    /// The consumer dropped the socket.
    Consumer,
    /// No terminal stayed attached through the idle grace.
    Idle,
    Failed {
        refused: bool,
    },
}

async fn run(
    client: ServerClient,
    timing: SocketTiming,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<TerminalEvent>,
) {
    let mut attachments = Attachments::default();
    let mut pending: VecDeque<(String, String)> = VecDeque::new();
    let mut failures: u32 = 0;
    loop {
        // Dial only while something is attached or waiting to be written.
        while attachments.last.is_empty() && pending.is_empty() {
            let Some(command) = commands.recv().await else {
                return;
            };
            apply_offline(command, &mut attachments, &mut pending);
        }
        let ended = match connect(&client, timing).await {
            Ok(socket) => {
                connected(
                    socket,
                    timing,
                    &mut commands,
                    &events,
                    &mut attachments,
                    &mut pending,
                    &mut failures,
                )
                .await
            }
            Err(refused) => Ended::Failed { refused },
        };
        let refused = match ended {
            Ended::Consumer => return,
            Ended::Idle => continue,
            Ended::Failed { refused } => refused,
        };
        // Input the server never received is reported, not replayed later.
        for (session, _) in pending.drain(..) {
            if events
                .send(TerminalEvent::WriteFailed { session })
                .await
                .is_err()
            {
                return;
            }
        }
        if attachments.last.is_empty() {
            failures = 0;
            continue;
        }
        failures = failures.saturating_add(1);
        let delay = if refused {
            timing.refused_delay
        } else {
            timing.backoff(failures)
        };
        if events
            .send(TerminalEvent::Reconnecting {
                attempt: failures,
                delay,
                refused,
            })
            .await
            .is_err()
        {
            return;
        }
        let wake = tokio::time::sleep(delay);
        tokio::pin!(wake);
        loop {
            tokio::select! {
                () = &mut wake => break,
                command = commands.recv() => {
                    let Some(command) = command else { return };
                    let write = matches!(command, Command::Write { .. });
                    apply_offline(command, &mut attachments, &mut pending);
                    // Input dials at once, as `write` calls `ensureConnected`.
                    if write {
                        break;
                    }
                    if attachments.last.is_empty() {
                        failures = 0;
                        break;
                    }
                }
            }
        }
    }
}

fn apply_offline(
    command: Command,
    attachments: &mut Attachments,
    pending: &mut VecDeque<(String, String)>,
) {
    match command {
        Command::Attach(session) => {
            attachments.last.insert(session, None);
        }
        Command::Detach(session) => {
            attachments.last.remove(&session);
        }
        Command::Write { session, data } => pending.push_back((session, data)),
    }
}

/// Opens the socket. `Err(true)` when the server refused the credentials.
async fn connect(client: &ServerClient, timing: SocketTiming) -> Result<Socket, bool> {
    let url = client.endpoint().websocket_url(SOCKET_ROUTE);
    let mut request = url.as_str().into_client_request().map_err(|_| false)?;
    request.headers_mut().insert(
        "origin",
        HeaderValue::from_str(&client.endpoint().url("").origin().ascii_serialization())
            .map_err(|_| false)?,
    );
    if let Some(value) = client.credential().authorization() {
        let mut value = HeaderValue::from_str(&value).map_err(|_| false)?;
        value.set_sensitive(true);
        request.headers_mut().insert("authorization", value);
    }
    let handshake = tokio::time::timeout(
        timing.open_timeout,
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| false)?;
    match handshake {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if matches!(response.status().as_u16(), 401 | 403) =>
        {
            Err(true)
        }
        Err(error) => {
            tracing::debug!(%error, "the terminal socket did not open");
            Err(false)
        }
    }
}

async fn send_frame(socket: &mut Socket, frame: &ClientFrame<'_>) -> bool {
    socket
        .send(WsMessage::Binary(encode(frame).into()))
        .await
        .is_ok()
}

async fn send_write(socket: &mut Socket, session: &str, data: &str) -> bool {
    for chunk in split_write(session, data) {
        if !send_frame(
            socket,
            &ClientFrame::Write {
                session,
                data: chunk,
            },
        )
        .await
        {
            return false;
        }
    }
    true
}

/// Serves one open socket until it fails, idles out or the consumer leaves.
async fn connected(
    mut socket: Socket,
    timing: SocketTiming,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &mpsc::Sender<TerminalEvent>,
    attachments: &mut Attachments,
    pending: &mut VecDeque<(String, String)>,
    failures: &mut u32,
) -> Ended {
    let failed = Ended::Failed { refused: false };
    if !send_frame(&mut socket, &ClientFrame::Hello).await {
        return failed;
    }
    // Every (re)open asks for fresh snapshots, so a terminal resumes from the
    // server's state and never from whatever the client missed.
    let sessions: Vec<String> = attachments.last.keys().cloned().collect();
    for session in &sessions {
        if !send_frame(&mut socket, &ClientFrame::Attach(session)).await {
            return failed;
        }
    }
    while let Some((session, data)) = pending.front() {
        if !send_write(&mut socket, session, data).await {
            return failed;
        }
        pending.pop_front();
    }
    if events.send(TerminalEvent::Connected).await.is_err() {
        return Ended::Consumer;
    }
    let mut keepalive = tokio::time::interval_at(
        tokio::time::Instant::now() + timing.keepalive,
        timing.keepalive,
    );
    let mut idle_deadline: Option<tokio::time::Instant> = None;
    loop {
        if attachments.last.is_empty() {
            idle_deadline.get_or_insert_with(|| tokio::time::Instant::now() + timing.idle_grace);
        } else {
            idle_deadline = None;
        }
        let idle = async {
            match idle_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            message = socket.next() => {
                let bytes = match message {
                    Some(Ok(WsMessage::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(WsMessage::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return failed,
                    Some(Ok(_)) => continue,
                };
                let Some(frame) = decode(&bytes) else { continue };
                if matches!(frame, ServerFrame::Hello) {
                    // The server speaks the protocol: the connection is healthy.
                    *failures = 0;
                }
                if let Some(event) = deliver(frame, attachments)
                    && events.send(event).await.is_err()
                {
                    return Ended::Consumer;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { return Ended::Consumer };
                let sent = match command {
                    Command::Attach(session) => {
                        let fresh = !attachments.last.contains_key(&session);
                        attachments.last.insert(session.clone(), None);
                        !fresh || send_frame(&mut socket, &ClientFrame::Attach(&session)).await
                    }
                    Command::Detach(session) => {
                        attachments.last.remove(&session).is_none()
                            || send_frame(&mut socket, &ClientFrame::Detach(&session)).await
                    }
                    Command::Write { session, data } => {
                        if send_write(&mut socket, &session, &data).await {
                            true
                        } else {
                            pending.push_back((session, data));
                            false
                        }
                    }
                };
                if !sent {
                    return failed;
                }
            }
            _ = keepalive.tick() => {
                if !send_frame(&mut socket, &ClientFrame::Ping).await {
                    return failed;
                }
            }
            () = idle => {
                let _ = socket.close(None).await;
                return Ended::Idle;
            }
        }
    }
}

/// Turns a decoded frame into the event the consumer sees, or nothing when
/// it is stale or for a terminal no longer attached.
fn deliver(frame: ServerFrame, attachments: &mut Attachments) -> Option<TerminalEvent> {
    if let Some((session, sequence)) = frame.sequenced() {
        let snapshot = matches!(frame, ServerFrame::Snapshot(_));
        if !attachments.accept(session, sequence, snapshot) {
            return None;
        }
    }
    Some(match frame {
        ServerFrame::Hello | ServerFrame::Pong => return None,
        ServerFrame::Snapshot(snapshot) => TerminalEvent::Snapshot(snapshot),
        ServerFrame::Output {
            session,
            sequence,
            data,
            ..
        } => TerminalEvent::Output {
            session,
            sequence,
            data,
        },
        ServerFrame::Exit {
            session,
            exit_code,
            signal,
            ..
        } => TerminalEvent::Exit {
            session,
            exit_code,
            signal,
        },
        ServerFrame::Restarted {
            session, history, ..
        } => TerminalEvent::Restarted { session, history },
        ServerFrame::Error {
            session,
            code,
            fatal,
            ..
        } => {
            if fatal && let Some(session) = &session {
                // The server already dropped this attachment.
                attachments.last.remove(session);
            }
            TerminalEvent::Error {
                session,
                code,
                fatal,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_len(session: &str, chunk: &str) -> usize {
        encode(&ClientFrame::Write {
            session,
            data: chunk,
        })
        .len()
    }

    #[test]
    fn frames_carry_the_tag_and_protocol_version() {
        let frame = encode(&ClientFrame::Attach("t1"));
        assert_eq!(frame[0], FRAME_TAG);
        let value: Value = serde_json::from_slice(&frame[1..]).expect("the payload is JSON");
        assert_eq!(value, json!({ "t": "attach", "v": 3, "s": "t1" }));
        let frame = encode(&ClientFrame::Write {
            session: "t1",
            data: "a\"\u{1}",
        });
        let value: Value = serde_json::from_slice(&frame[1..]).expect("the payload is JSON");
        assert_eq!(
            value,
            json!({ "t": "write", "v": 3, "s": "t1", "d": "a\"\u{1}" })
        );
    }

    #[test]
    fn decodes_each_server_frame_and_rejects_incomplete_ones() {
        let snapshot = decode(
            b"\x01{\"t\":\"snapshot\",\"v\":3,\"s\":\"a\",\"q\":41,\"history\":\"h\",\"cols\":120,\"rows\":32,\"status\":\"running\",\"exitCode\":null,\"signal\":null}",
        );
        assert_eq!(
            snapshot,
            Some(ServerFrame::Snapshot(Snapshot {
                session: "a".to_owned(),
                sequence: 41,
                history: "h".to_owned(),
                cols: Some(120),
                rows: Some(32),
                status: TerminalStatus::Running,
                exit_code: None,
                signal: None,
            }))
        );
        assert_eq!(
            decode(b"\x01{\"t\":\"output\",\"v\":3,\"s\":\"a\",\"q\":42,\"d\":\"x\",\"r\":\"y\"}"),
            Some(ServerFrame::Output {
                session: "a".to_owned(),
                sequence: 42,
                data: "x".to_owned(),
                replay: Some("y".to_owned()),
            })
        );
        assert_eq!(
            decode(b"\x01{\"t\":\"exit\",\"v\":3,\"s\":\"a\",\"q\":43,\"exitCode\":3}"),
            Some(ServerFrame::Exit {
                session: "a".to_owned(),
                sequence: 43,
                exit_code: Some(3),
                signal: None,
            })
        );
        assert_eq!(
            decode(b"\x01{\"t\":\"error\",\"v\":3,\"s\":\"a\",\"code\":\"CLOSED\",\"fatal\":true}"),
            Some(ServerFrame::Error {
                session: Some("a".to_owned()),
                code: Some("CLOSED".to_owned()),
                message: None,
                fatal: true,
            })
        );
        assert_eq!(
            decode(b"\x01{\"t\":\"output\",\"s\":\"a\",\"d\":\"x\"}"),
            None
        );
        assert_eq!(decode(b"\x01{\"t\":\"snapshot\",\"s\":\"a\"}"), None);
        assert_eq!(decode(b"\x01not json"), None);
        assert_eq!(decode(b"\x01{\"t\":\"surprise\"}"), None);
    }

    #[test]
    fn writes_split_under_the_frame_limit_without_losing_bytes() {
        let session = "0f6c3d1e-5a9b-4c1e-9d2a-7b8c9d0e1f2a";
        let ascii = "a".repeat(200 * 1024);
        let chunks = split_write(session, &ascii);
        assert!(chunks.len() > 3, "200 KiB needs several frames");
        assert_eq!(chunks.concat(), ascii);
        assert!(
            chunks
                .iter()
                .all(|chunk| frame_len(session, chunk) <= MAX_FRAME_BYTES)
        );

        // Control bytes expand sixfold in JSON; the budget counts the escaped form.
        let controls = "\u{1}".repeat(40 * 1024);
        let chunks = split_write(session, &controls);
        assert_eq!(chunks.concat(), controls);
        assert!(
            chunks
                .iter()
                .all(|chunk| frame_len(session, chunk) <= MAX_FRAME_BYTES)
        );

        // Astral characters count two UTF-16 units and never split.
        let wide = "\u{1f600}好".repeat(30_000);
        let chunks = split_write(session, &wide);
        assert_eq!(chunks.concat(), wide);
        for chunk in &chunks {
            assert!(frame_len(session, chunk) <= MAX_FRAME_BYTES);
            assert!(chunk.encode_utf16().count() <= MAX_WRITE_UNITS);
        }

        assert_eq!(split_write(session, "ls\r"), vec!["ls\r"]);
        assert!(split_write(session, "").is_empty());
    }

    #[test]
    fn sequences_drop_stale_events_and_snapshots_reset_them() {
        let mut attachments = Attachments::default();
        attachments.last.insert("a".to_owned(), None);
        let output = |sequence| ServerFrame::Output {
            session: "a".to_owned(),
            sequence,
            data: format!("{sequence}"),
            replay: None,
        };
        assert!(deliver(output(5), &mut attachments).is_some());
        assert!(
            deliver(output(5), &mut attachments).is_none(),
            "a repeat is dropped"
        );
        assert!(
            deliver(output(4), &mut attachments).is_none(),
            "an older event is dropped"
        );
        let snapshot = ServerFrame::Snapshot(Snapshot {
            session: "a".to_owned(),
            sequence: 9,
            history: String::new(),
            cols: None,
            rows: None,
            status: TerminalStatus::Running,
            exit_code: None,
            signal: None,
        });
        assert!(deliver(snapshot, &mut attachments).is_some());
        assert!(
            deliver(output(9), &mut attachments).is_none(),
            "the snapshot covers 9"
        );
        assert!(deliver(output(10), &mut attachments).is_some());
        let detached = ServerFrame::Output {
            session: "b".to_owned(),
            sequence: 1,
            data: String::new(),
            replay: None,
        };
        assert!(deliver(detached, &mut attachments).is_none());
    }

    #[test]
    fn backoff_doubles_to_its_cap() {
        let timing = SocketTiming::default();
        assert_eq!(timing.backoff(1), Duration::from_millis(500));
        assert_eq!(timing.backoff(2), Duration::from_secs(1));
        assert_eq!(timing.backoff(4), Duration::from_secs(4));
        assert_eq!(timing.backoff(5), Duration::from_secs(8));
        assert_eq!(timing.backoff(40), Duration::from_secs(8));
    }

    #[test]
    fn settings_keep_defaults_for_malformed_values() {
        let settings = parse_settings(&json!({
            "terminalCloseOnExit": false,
            "terminalFontSize": "big",
            "terminalShell": "zsh",
            "terminalLoginShells": ["zsh", 4],
        }));
        assert!(!settings.close_on_exit);
        assert_eq!(settings.font_size, 14.0);
        assert_eq!(settings.shell, "zsh");
        assert_eq!(settings.login_shells, vec!["zsh".to_owned()]);
        assert_eq!(
            parse_settings(&json!({"terminalFontSize": 100})).font_size,
            32.0
        );
    }
}
