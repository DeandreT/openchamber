//! The configured OpenChamber server and the state of the connection to it.

use std::fmt;

use gpui_kit::Task;
use openchamber_native_transport::client::{ClientDescription, ServerClient};
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::error::TransportError;

use crate::install_id;
use crate::network::Network;

/// `OPENCHAMBER_NATIVE_SERVER`: the base URL of the server to connect to.
const SERVER_VARIABLE: &str = "OPENCHAMBER_NATIVE_SERVER";
/// `OPENCHAMBER_NATIVE_PASSWORD`: the UI password of a password server.
const PASSWORD_VARIABLE: &str = "OPENCHAMBER_NATIVE_PASSWORD";

/// The password variable's value, read once and removed from the
/// environment on Linux, so WebKit's processes and every other child started
/// later cannot inherit it.
static PASSWORD: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Takes `OPENCHAMBER_NATIVE_PASSWORD` out of the environment. `main` calls
/// this before any thread starts, as changing the environment requires.
pub(crate) fn take_password_from_environment() {
    PASSWORD.get_or_init(|| {
        let password = std::env::var(PASSWORD_VARIABLE)
            .ok()
            .filter(|password| !password.is_empty());
        #[cfg(target_os = "linux")]
        gtk::glib::unsetenv(PASSWORD_VARIABLE);
        password
    });
}

fn environment_password() -> Option<String> {
    PASSWORD
        .get_or_init(|| {
            std::env::var(PASSWORD_VARIABLE)
                .ok()
                .filter(|password| !password.is_empty())
        })
        .clone()
}

/// An explicitly selected server.
#[derive(Clone)]
pub struct ServerConfig {
    pub endpoint: Endpoint,
    /// Exchanged once for a client token; never logged.
    pub password: Option<String>,
    /// What the server records about this install when it issues a token.
    pub client: ClientDescription,
}

impl ServerConfig {
    pub(crate) fn for_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            password: None,
            client: describe_client(),
        }
    }
    /// The server named in the environment, if one is.
    pub fn from_env() -> Option<Result<Self, TransportError>> {
        let url = std::env::var(SERVER_VARIABLE).ok()?;
        let password = environment_password();
        Some(Endpoint::parse(&url).map(|endpoint| Self {
            endpoint,
            password,
            client: describe_client(),
        }))
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("endpoint", &self.endpoint)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("client", &self.client)
            .finish()
    }
}

/// This install as a desktop connected to a remote server, described the way
/// the Electron desktop describes itself.
fn describe_client() -> ClientDescription {
    let platform = match std::env::consts::OS {
        "macos" => Some("macos"),
        "windows" => Some("windows"),
        "linux" => Some("linux"),
        _ => None,
    };
    ClientDescription {
        label: "OpenChamber Desktop".to_owned(),
        kind: "desktop".to_owned(),
        dedupe_key: install_id::load_or_create().map(|id| format!("desktop:{id}")),
        device_platform: platform.map(str::to_owned),
        app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
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
    ClientAuthenticationRequired,
    AuthenticationRateLimited { retry_after_seconds: Option<u64> },
}

impl ConnectionFailure {
    pub fn from_error(error: &TransportError) -> Self {
        match error {
            TransportError::NotOpenChamber => Self::NotOpenChamber,
            TransportError::Incompatible(_) => Self::Incompatible,
            TransportError::AuthenticationRequired => Self::AuthenticationRequired,
            TransportError::AuthenticationRejected => Self::AuthenticationRejected,
            TransportError::ClientAuthenticationRequired => Self::ClientAuthenticationRequired,
            TransportError::AuthenticationRateLimited {
                retry_after_seconds,
            } => Self::AuthenticationRateLimited {
                retry_after_seconds: *retry_after_seconds,
            },
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
    pub(crate) display_label: Option<String>,
    pub(crate) focus_on_ready: bool,
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
    pub control_task: Option<Task<()>>,
    pub authentication_task: Option<Task<()>>,
    pub sessions_task: Option<Task<()>>,
    pub topology_task: Option<Task<()>>,
    /// A pending retry after a transient failure. It survives `restart`,
    /// which the retry itself calls.
    pub retry_task: Option<Task<()>>,
    /// The host's OpenCode compatibility. It survives `restart`: a failed
    /// reconnect never dismisses a known incompatible answer.
    pub(crate) compatibility: crate::compatibility_view::CompatibilityGate,
    /// The compatibility check or recovery in flight. Its results check the
    /// gate's revision.
    pub(crate) compatibility_task: Option<Task<()>>,
}

impl Connection {
    /// Drops obsolete reads and subscriptions while preserving the executor
    /// that captured mutation chains may still be using.
    pub(crate) fn into_network(self) -> Network {
        self.network
    }

    /// A launch-supplied password is authorization for one exchange only.
    pub(crate) fn take_attempt_config(&mut self) -> ServerConfig {
        let mut config = self.config.clone();
        config.password = self.config.password.take();
        config
    }

    pub fn new(config: ServerConfig) -> std::io::Result<Self> {
        Ok(Self {
            config,
            display_label: None,
            focus_on_ready: false,
            network: Network::new()?,
            client: None,
            state: ConnectionState::Connecting,
            generation: 0,
            loaded: false,
            stale: false,
            connect_task: None,
            stream_task: None,
            control_task: None,
            authentication_task: None,
            sessions_task: None,
            topology_task: None,
            retry_task: None,
            compatibility: Default::default(),
            compatibility_task: None,
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
        self.control_task = None;
        self.authentication_task = None;
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
