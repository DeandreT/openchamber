//! The configured OpenChamber server and the state of the connection to it.

use std::fmt;

use gpui_kit::Task;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::error::TransportError;

use crate::network::Network;

/// `OPENCHAMBER_NATIVE_SERVER`: the base URL of the server to connect to.
const SERVER_VARIABLE: &str = "OPENCHAMBER_NATIVE_SERVER";
/// `OPENCHAMBER_NATIVE_PASSWORD`: the UI password of a password server.
const PASSWORD_VARIABLE: &str = "OPENCHAMBER_NATIVE_PASSWORD";

/// An explicitly selected server.
#[derive(Clone)]
pub struct ServerConfig {
    pub endpoint: Endpoint,
    /// Exchanged once for a client token; never logged.
    pub password: Option<String>,
}

impl ServerConfig {
    /// The server named in the environment, if one is.
    pub fn from_env() -> Option<Result<Self, TransportError>> {
        let url = std::env::var(SERVER_VARIABLE).ok()?;
        let password = std::env::var(PASSWORD_VARIABLE)
            .ok()
            .filter(|password| !password.is_empty());
        Some(Endpoint::parse(&url).map(|endpoint| Self { endpoint, password }))
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("endpoint", &self.endpoint)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Why the connection is unusable, as the views explain it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionFailure {
    Unreachable,
    NotOpenChamber,
    Incompatible,
    AuthenticationRequired,
    AuthenticationRejected,
}

impl ConnectionFailure {
    pub fn from_error(error: &TransportError) -> Self {
        match error {
            TransportError::NotOpenChamber => Self::NotOpenChamber,
            TransportError::Incompatible(_) => Self::Incompatible,
            TransportError::AuthenticationRequired => Self::AuthenticationRequired,
            TransportError::AuthenticationRejected => Self::AuthenticationRejected,
            _ => Self::Unreachable,
        }
    }

    /// Whether trying again later can succeed without the user's help.
    pub fn is_transient(self) -> bool {
        self == Self::Unreachable
    }
}

/// Where the connection stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    /// The event stream dropped; the last valid state stays on screen.
    Reconnecting,
    Failed(ConnectionFailure),
}

/// The live connection: its runtime, client and the tasks that use them.
///
/// Dropping it cancels every request and the subscription. It never stops
/// the server or a running turn.
pub struct Connection {
    pub config: ServerConfig,
    pub network: Network,
    /// Present once the identity check and authentication succeeded.
    pub client: Option<ServerClient>,
    pub state: ConnectionState,
    /// Bumps on every connection attempt; results of older attempts are
    /// dropped.
    pub generation: u64,
    /// The global reads ran for this attempt.
    pub loaded: bool,
    /// The stream dropped since it last connected.
    pub stale: bool,
    pub connect_task: Option<Task<()>>,
    pub stream_task: Option<Task<()>>,
    pub sessions_task: Option<Task<()>>,
    pub topology_task: Option<Task<()>>,
    /// A pending retry after a transient failure. It survives `restart`,
    /// which the retry itself calls.
    pub retry_task: Option<Task<()>>,
}

impl Connection {
    pub fn new(config: ServerConfig) -> std::io::Result<Self> {
        Ok(Self {
            config,
            network: Network::new()?,
            client: None,
            state: ConnectionState::Connecting,
            generation: 0,
            loaded: false,
            stale: false,
            connect_task: None,
            stream_task: None,
            sessions_task: None,
            topology_task: None,
            retry_task: None,
        })
    }

    /// Cancels the work of the previous attempt and starts a new generation.
    pub fn restart(&mut self) -> u64 {
        self.generation += 1;
        self.client = None;
        self.state = ConnectionState::Connecting;
        self.loaded = false;
        self.stale = false;
        self.connect_task = None;
        self.stream_task = None;
        self.sessions_task = None;
        self.topology_task = None;
        self.generation
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("config", &self.config)
            .field("state", &self.state)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
