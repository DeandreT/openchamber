//! The local OpenChamber server the desktop starts when no external host is
//! configured.
//!
//! The native client starts its bundled Bun as an owned child. Bun loads the
//! packaged `@openchamber/web` files and calls `startWebUiServer`. That server
//! starts and owns OpenCode.
//!
//! The listener is always loopback. The desktop binds a network address only
//! for its LAN access setting, which enterprise mode can refuse; the native
//! client has no such setting, so the managed server never leaves this
//! machine and enterprise mode has nothing to widen. The server reads the
//! policy file and variables itself, from the environment it inherits.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::managed_host_lifecycle::{HostFailure, HostState};
use crate::managed_host_supervisor::{Launch, Message, StopOutcome, Supervisor, Timeouts};

/// Optional native bundle directory. Defaults to the native binary's directory.
pub(crate) const BUNDLE_VARIABLE: &str = "OPENCHAMBER_NATIVE_BUNDLE";
/// The variable that carries the launch to the bootstrap, which removes it.
const LAUNCH_VARIABLE: &str = "OPENCHAMBER_NATIVE_HOST_LAUNCH";
/// The record of injected variable names (`server/lib/injected-env.js`).
const INJECTED_ENV_KEY: &str = "OPENCHAMBER_INJECTED_ENV";
const LOOPBACK: &str = "127.0.0.1";
const PORT_FILE: &str = "managed-host.json";
const LOG_FILE: &str = "managed-host.log";
/// The log is set aside once it grows past this size.
const LOG_LIMIT: u64 = 5 * 1024 * 1024;

/// Starts the server with the options `spawnLocalServer` passes, stops it on
/// end of input or a signal, and prints `<nonce> ready <port>` once it
/// listens. The launch variable is removed before the server loads.
const BOOTSTRAP: &str = r#"'use strict';
const { pathToFileURL } = require('node:url');
const launch = JSON.parse(process.env.OPENCHAMBER_NATIVE_HOST_LAUNCH);
delete process.env.OPENCHAMBER_NATIVE_HOST_LAUNCH;
let handle = null;
let stopping = false;
const stop = async () => {
  if (stopping) return;
  stopping = true;
  // During startup the server stops as soon as it has started.
  if (handle === null) return;
  try {
    await handle.stop({ exitProcess: false });
  } catch (error) {
    console.error('[native-host] shutdown failed:', error);
    process.exit(1);
  }
  process.exit(0);
};
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(signal, () => { void stop(); });
process.stdin.on('end', () => { void stop(); });
process.stdin.on('error', () => { void stop(); });
process.stdin.resume();
import(pathToFileURL(launch.entry).href)
  .then((server) => server.startWebUiServer({
    port: launch.port,
    host: '127.0.0.1',
    uiPassword: null,
    attachSignals: false,
    exitOnShutdown: false,
    apiOnly: false,
    builtInExtensionsDir: launch.builtInExtensionsDir,
    resourcesPath: launch.resourcesPath,
    nativeDesktopExecutable: launch.nativeDesktopExecutable,
  }))
  .then((started) => {
    handle = started;
    if (stopping) {
      stopping = false;
      return stop();
    }
    process.stdout.write(`\n${launch.nonce} ready ${started.getPort()}\n`);
  })
  .catch((error) => {
    console.error('[native-host] the server did not start:', error);
    process.exit(1);
  });
"#;

/// The native package layout the server runs from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bundle {
    root: PathBuf,
}

/// What a bundle is missing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryError {
    /// Neither the variable nor the binary names a directory.
    NoCandidate,
    /// The layout lacks this file.
    Missing(PathBuf),
}

impl Bundle {
    fn resources(&self) -> PathBuf {
        self.root.join("resources")
    }

    fn runtime(&self) -> PathBuf {
        self.resources()
            .join("bun")
            .join(if cfg!(windows) { "bun.exe" } else { "bun" })
    }

    fn server_package(&self) -> PathBuf {
        self.resources().join("server")
    }

    fn server_entry(&self) -> PathBuf {
        self.server_package().join("server/index.js")
    }

    fn built_in_extensions(&self) -> PathBuf {
        self.server_package().join("server/built-in-extensions")
    }

    fn web_dist(&self) -> PathBuf {
        self.resources().join("web-dist")
    }

    fn opencode_dir(&self) -> PathBuf {
        self.resources().join("opencode-cli")
    }

    fn opencode(&self) -> PathBuf {
        self.opencode_dir().join(if cfg!(windows) {
            "opencode.exe"
        } else {
            "opencode"
        })
    }
}

/// The bundle named by the variable, else the one beside the binary. A named
/// bundle that is incomplete is an error; nothing falls back past it.
pub(crate) fn discover(
    named: Option<PathBuf>,
    executable: Option<PathBuf>,
) -> Result<Bundle, DiscoveryError> {
    let root = named
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| executable.and_then(|path| path.parent().map(Path::to_path_buf)))
        .ok_or(DiscoveryError::NoCandidate)?;
    let bundle = Bundle {
        root: fs::canonicalize(&root).map_err(|_| DiscoveryError::Missing(root.clone()))?,
    };
    for (path, executable) in [
        (bundle.runtime(), true),
        (bundle.server_package().join("package.json"), false),
        (bundle.server_entry(), false),
        (bundle.web_dist().join("index.html"), false),
        (bundle.opencode(), true),
    ] {
        if !is_file(&path, executable) {
            return Err(DiscoveryError::Missing(path));
        }
    }
    Ok(bundle)
}

fn is_file(path: &Path, executable: bool) -> bool {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| {
        #[cfg(unix)]
        let runnable = !executable || metadata.permissions().mode() & 0o111 != 0;
        #[cfg(windows)]
        let runnable = !executable
            || path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
        metadata.is_file() && runnable
    })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_server_launch_preserves_proxy_bypass_and_case_insensitive_environment() {
        let bundle = Bundle {
            root: PathBuf::from("C:/synthetic bundle"),
        };
        for spelling in ["NO_PROXY", "no_proxy", "No_PrOxY"] {
            let inherited = [
                (spelling, "internal.example"),
                ("openchamber_native_password", "synthetic-filtered-value"),
                ("electron_run_as_node", "old"),
                ("openchamber_host", "0.0.0.0"),
                ("openchamber_skip_api_compression", "false"),
            ]
            .map(|(key, value)| (OsString::from(key), OsString::from(value)));
            let launch = server_launch(&bundle, 4321, "fixture".to_owned(), inherited, None, None);
            let proxy: Vec<_> = launch
                .env
                .iter()
                .filter(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("NO_PROXY"))
                .collect();
            assert_eq!(
                proxy.len(),
                1,
                "one effective Windows proxy bypass for {spelling}"
            );
            assert_eq!(proxy[0].1, "internal.example,localhost,127.0.0.1");
            let mut command = std::process::Command::new(&launch.program);
            command.env_clear().envs(launch.env);
            let effective: Vec<_> = command.get_envs().collect();
            let value = |key: &str| {
                effective
                    .iter()
                    .find(|(name, _)| name.to_string_lossy().eq_ignore_ascii_case(key))
                    .and_then(|(_, value)| *value)
            };
            assert_eq!(
                value("NO_PROXY"),
                Some(OsStr::new("internal.example,localhost,127.0.0.1"))
            );
            assert_eq!(value("OPENCHAMBER_NATIVE_PASSWORD"), None);
            assert_eq!(value("OPENCHAMBER_HOST"), Some(OsStr::new("127.0.0.1")));
            assert_eq!(
                value("OPENCHAMBER_SKIP_API_COMPRESSION"),
                Some(OsStr::new("false"))
            );
            assert_eq!(value("ELECTRON_RUN_AS_NODE"), None);
        }
    }

    #[test]
    fn windows_bundle_uses_packaged_executables_and_kernel_nonce() {
        let root = std::env::temp_dir().join(format!("oc-win-bundle-{}", std::process::id()));
        for name in [
            "resources/bun/bun.exe",
            "resources/server/package.json",
            "resources/server/server/index.js",
            "resources/web-dist/index.html",
            "resources/opencode-cli/opencode.exe",
        ] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().expect("fixture parent"))
                .expect("fixture directories");
            fs::write(path, "").expect("synthetic package file");
        }
        let bundle = discover(Some(root.clone()), None).expect("Windows package discovery");
        assert!(bundle.runtime().ends_with("bun/bun.exe"));
        assert!(bundle.opencode().ends_with("opencode.exe"));
        assert_eq!(random_nonce().expect("kernel nonce").len(), 32);
        fs::remove_file(bundle.runtime()).expect("remove fixture runtime");
        assert!(matches!(
            discover(Some(root.clone()), None),
            Err(DiscoveryError::Missing(_))
        ));
        fs::remove_dir_all(root).expect("remove synthetic package");
    }

    #[test]
    #[ignore = "set OPENCHAMBER_TEST_BUN to the pinned Windows bun.exe"]
    fn windows_bun_enters_the_server_module_from_a_canonical_cwd() {
        let bun = PathBuf::from(std::env::var_os("OPENCHAMBER_TEST_BUN").expect("test Bun"));
        assert!(bun.is_absolute());
        let nonce = random_nonce().expect("test nonce");
        let root = std::env::temp_dir().join(format!("oc Bun # é {nonce}"));
        fs::create_dir_all(root.join("resources/server/server")).expect("fixture directories");
        let bundle = Bundle {
            root: fs::canonicalize(&root).expect("canonical bundle"),
        };
        fs::write(
            bundle.server_package().join("package.json"),
            r#"{"type":"module"}"#,
        )
        .expect("fixture package");
        // A distinct status proves the bootstrap called the entry, even when Bun
        // would otherwise exit successfully without evaluating the script.
        fs::write(
            bundle.server_entry(),
            "export function startWebUiServer() { process.exit(57); }",
        )
        .expect("fixture module");
        let env = [
            (
                "SystemRoot".into(),
                std::env::var_os("SystemRoot").expect("Windows system root"),
            ),
            ("USERPROFILE".into(), root.as_os_str().into()),
            ("TEMP".into(), root.as_os_str().into()),
        ];
        let launch = server_launch(&bundle, 1, nonce, env, None, None);
        let child = openchamber_windows_process::spawn(&bun, &launch.args, &launch.env)
            .expect("run bundled bootstrap");
        let empty = child.job.wait_empty(std::time::Duration::from_secs(10));
        let status = empty.and_then(|done| done.then(|| child.process.wait()).transpose());
        drop(child);
        fs::remove_dir_all(root).expect("remove fixture");
        assert_eq!(status.expect("observe child"), Some(57));
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchOptions<'a> {
    entry: &'a Path,
    resources_path: &'a Path,
    #[cfg(target_os = "linux")]
    native_desktop_executable: Option<PathBuf>,
    port: u16,
    built_in_extensions_dir: &'a Path,
    nonce: &'a str,
}

/// The server's environment: the inherited one with the desktop's
/// additions, recorded as injected the way `assignInjectedEnv` records them.
fn server_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    bundle: &Bundle,
    home: Option<&OsStr>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            #[cfg(windows)]
            let key = key.to_ascii_uppercase();
            // The native client's own settings, a password the desktop only
            // passes with LAN access, and a cache the desktop keeps out of
            // child environments.
            !key.starts_with("OPENCHAMBER_NATIVE_")
                && key != "OPENCHAMBER_UI_PASSWORD"
                && key != "OPENCHAMBER_DESKTOP_LAN_ACCESS_BLOCKED_REASON"
                && key != "NODE_COMPILE_CACHE"
                && key != "ELECTRON_RUN_AS_NODE"
        })
        .collect();
    let existing = |env: &[(OsString, OsString)], key: &str| {
        let entries = env.iter();
        #[cfg(windows)]
        let mut entries = entries.rev();
        #[cfg(unix)]
        let mut entries = entries;
        entries
            .find(|(name, _)| environment_key_eq(name, key))
            .map(|(_, value)| value.clone())
    };
    let opencode_cwd = existing(&env, "OPENCHAMBER_OPENCODE_CWD")
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(OsStr::to_os_string)
        });
    let compression =
        existing(&env, "OPENCHAMBER_SKIP_API_COMPRESSION").unwrap_or_else(|| "true".into());
    #[cfg(unix)]
    let no_proxy = existing(&env, "NO_PROXY").unwrap_or_else(|| "localhost,127.0.0.1".into());
    #[cfg(unix)]
    let no_proxy_lower = existing(&env, "no_proxy").unwrap_or_else(|| "localhost,127.0.0.1".into());
    #[cfg(windows)]
    let no_proxy = {
        let mut value = existing(&env, "NO_PROXY").unwrap_or_default();
        for host in ["localhost", "127.0.0.1"] {
            if !value
                .to_string_lossy()
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case(host))
            {
                if !value.is_empty() {
                    value.push(",");
                }
                value.push(host);
            }
        }
        value
    };
    let assignments: Vec<(&str, Option<OsString>)> = vec![
        ("OPENCHAMBER_HOST", Some(LOOPBACK.into())),
        (
            "OPENCHAMBER_DESKTOP_LAN_ACCESS_ACTIVE",
            Some("false".into()),
        ),
        ("OPENCHAMBER_DIST_DIR", Some(bundle.web_dist().into())),
        (
            "OPENCHAMBER_BUNDLED_OPENCODE_CLI_DIR",
            Some(bundle.opencode_dir().into()),
        ),
        ("OPENCHAMBER_RUNTIME", Some("desktop".into())),
        ("OPENCHAMBER_OPENCODE_CWD", opencode_cwd),
        ("OPENCHAMBER_DESKTOP_NOTIFY", Some("true".into())),
        ("OPENCHAMBER_SKIP_API_COMPRESSION", Some(compression)),
        ("NO_PROXY", Some(no_proxy)),
        #[cfg(unix)]
        ("no_proxy", Some(no_proxy_lower)),
    ];
    assign_injected(&mut env, assignments);
    env
}

fn environment_key_eq(name: &OsStr, key: &str) -> bool {
    #[cfg(windows)]
    {
        name.to_string_lossy().eq_ignore_ascii_case(key)
    }
    #[cfg(unix)]
    {
        name == key
    }
}

/// Sets one effective Windows key, preserving Unix's case-sensitive behavior.
fn set_environment(env: &mut Vec<(OsString, OsString)>, key: &str, value: OsString) -> bool {
    #[cfg(windows)]
    {
        let changed = env
            .iter()
            .rfind(|(name, _)| environment_key_eq(name, key))
            .is_none_or(|(_, current)| *current != value);
        env.retain(|(name, _)| !environment_key_eq(name, key));
        env.push((key.into(), value));
        changed
    }
    #[cfg(unix)]
    {
        match env.iter_mut().find(|(name, _)| name == key) {
            Some((_, current)) if *current == value => false,
            Some((_, current)) => {
                *current = value;
                true
            }
            None => {
                env.push((key.into(), value));
                true
            }
        }
    }
}

/// `assignInjectedEnv`: set each defined value and record the names whose
/// value changed, after the names already recorded.
fn assign_injected(env: &mut Vec<(OsString, OsString)>, values: Vec<(&str, Option<OsString>)>) {
    let entries = env.iter();
    #[cfg(windows)]
    let mut entries = entries.rev();
    #[cfg(unix)]
    let mut entries = entries;
    let mut recorded: Vec<String> = entries
        .find(|(key, _)| environment_key_eq(key, INJECTED_ENV_KEY))
        .map(|(_, value)| {
            value
                .to_string_lossy()
                .split(',')
                .filter(|name| {
                    !name.is_empty() && !environment_key_eq(OsStr::new(name), INJECTED_ENV_KEY)
                })
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    for (key, value) in values {
        let Some(value) = value else {
            continue;
        };
        if !set_environment(env, key, value) {
            continue;
        }
        if !recorded
            .iter()
            .any(|name| environment_key_eq(OsStr::new(name), key))
        {
            recorded.push(key.to_owned());
        }
    }
    if recorded.is_empty() {
        return;
    }
    let record = OsString::from(recorded.join(","));
    set_environment(env, INJECTED_ENV_KEY, record);
}

/// The complete launch of the bundled server on `port`.
fn server_launch(
    bundle: &Bundle,
    port: u16,
    nonce: String,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    home: Option<&OsStr>,
    log: Option<PathBuf>,
) -> Launch {
    let mut env = server_environment(inherited, bundle, home);
    let options = LaunchOptions {
        entry: &bundle.server_entry(),
        resources_path: &bundle.resources(),
        // A development bundle override must not claim an installed app's updates.
        #[cfg(target_os = "linux")]
        native_desktop_executable: std::env::current_exe()
            .ok()
            .filter(|exe| exe.parent() == Some(bundle.root.as_path())),
        port,
        built_in_extensions_dir: &bundle.built_in_extensions(),
        nonce: &nonce,
    };
    let options = serde_json::to_string(&options).expect("launch options serialize");
    env.push((LAUNCH_VARIABLE.into(), options.into()));
    let mut bootstrap = OsString::from(BOOTSTRAP);
    if cfg!(windows) {
        // With a verbatim package cwd, Bun 1.4.4 can exit before running a CommonJS
        // eval that uses require(). Mark it as a module without rewriting the path.
        bootstrap.push("export {};\n");
    }
    Launch {
        program: bundle.runtime(),
        // Bun reads configuration from its working directory. A project the
        // desktop was launched from must not configure the bundled server.
        args: vec![
            "--cwd".into(),
            bundle.server_package().into(),
            "--no-env-file".into(),
            "--no-install".into(),
            "-e".into(),
            bootstrap,
        ],
        env,
        port,
        nonce,
        log,
    }
}

/// The loopback port the managed server listened on last, like the
/// desktop's `desktopLocalPort`. A stable port keeps the runtime key, and
/// with it drafts and queued sends, the same across launches.
#[derive(Debug)]
pub(crate) struct PortStore {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct StoredPort {
    port: u16,
}

impl PortStore {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            path: directory.join(PORT_FILE),
        }
    }

    fn load(&self) -> Option<u16> {
        let text = fs::read_to_string(&self.path).ok()?;
        let stored: StoredPort = serde_json::from_str(&text).ok()?;
        (stored.port != 0).then_some(stored.port)
    }

    fn save(&self, port: u16) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let partial = self.path.with_extension("partial");
        let text = serde_json::to_string(&StoredPort { port }).map_err(io::Error::other)?;
        fs::write(&partial, text)?;
        fs::rename(&partial, &self.path)
    }

    /// The stored port while it is free, else a new free one. Its own
    /// default: the desktop's 57123 belongs to the desktop's profile.
    pub(crate) fn choose(&self) -> io::Result<u16> {
        if let Some(port) = self.load()
            && TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
        {
            return Ok(port);
        }
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        if let Err(error) = self.save(port) {
            tracing::warn!(%error, "the managed host port could not be saved");
        }
        Ok(port)
    }
}

/// 128 random bits from the kernel, as hex.
fn random_nonce() -> io::Result<String> {
    #[cfg(unix)]
    let bytes = {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        bytes
    };
    #[cfg(windows)]
    let bytes = openchamber_windows_process::random_nonce()?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Sets a large log aside before a new launch appends to it.
fn prepare_log(directory: &Path) -> Option<PathBuf> {
    let logs = directory.join("logs");
    fs::create_dir_all(&logs).ok()?;
    let path = logs.join(LOG_FILE);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LOG_LIMIT) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    Some(path)
}

/// The managed host for one launch, owned by `main`.
///
/// Dropping it stops the server and waits for its owned processes.
#[derive(Debug)]
pub(crate) struct ManagedHost {
    port: u16,
    state: watch::Receiver<HostState>,
    commands: Option<Sender<Message>>,
    thread: Option<JoinHandle<()>>,
    stopped: Arc<Mutex<Option<StopOutcome>>>,
}

impl ManagedHost {
    /// Starts the bundled server, or a failed host that explains why not.
    pub(crate) fn start(config_directory: Option<&Path>) -> Self {
        let named = std::env::var_os(BUNDLE_VARIABLE).map(PathBuf::from);
        let bundle = discover(named, std::env::current_exe().ok());
        let ports = config_directory.map(PortStore::new);
        let port = ports.as_ref().map_or_else(
            || {
                TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                    .and_then(|listener| listener.local_addr())
                    .map(|address| address.port())
            },
            PortStore::choose,
        );
        let (bundle, port) = match (bundle, port) {
            (Ok(bundle), Ok(port)) => (bundle, port),
            (Err(error), port) => {
                tracing::error!(?error, "the bundled server was not found");
                return Self::failed(port.unwrap_or_default(), HostFailure::BundleMissing);
            }
            (_, Err(error)) => {
                tracing::error!(%error, "no loopback port is free");
                return Self::failed(0, HostFailure::SpawnFailed);
            }
        };
        let nonce = match random_nonce() {
            Ok(nonce) => nonce,
            Err(error) => {
                tracing::error!(%error, "the managed host could not be prepared");
                return Self::failed(port, HostFailure::SpawnFailed);
            }
        };
        let log = config_directory.and_then(prepare_log);
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
        let launch = server_launch(
            &bundle,
            port,
            nonce,
            std::env::vars_os(),
            home.as_deref(),
            log,
        );
        Self::spawn(launch, Timeouts::default())
    }

    fn spawn(launch: Launch, timeouts: Timeouts) -> Self {
        let port = launch.port;
        let (state, receiver) = watch::channel(HostState::Starting);
        let (commands, messages) = mpsc::channel();
        let stopped = Arc::new(Mutex::new(None));
        let supervisor = Supervisor::new(
            launch,
            timeouts,
            state,
            messages,
            commands.clone(),
            stopped.clone(),
        );
        let thread = std::thread::Builder::new()
            .name("managed-host".to_owned())
            .spawn(move || supervisor.run());
        match thread {
            Ok(thread) => Self {
                port,
                state: receiver,
                commands: Some(commands),
                thread: Some(thread),
                stopped,
            },
            Err(error) => {
                tracing::error!(%error, "the managed host thread could not start");
                Self::failed(port, HostFailure::SpawnFailed)
            }
        }
    }

    fn failed(port: u16, failure: HostFailure) -> Self {
        let (_, state) = watch::channel(HostState::Failed(failure));
        Self {
            port,
            state,
            commands: None,
            thread: None,
            stopped: Arc::new(Mutex::new(None)),
        }
    }

    /// The loopback port the server listens on once ready.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Follows the host's state.
    pub(crate) fn subscribe(&self) -> watch::Receiver<HostState> {
        self.state.clone()
    }

    /// Stops the server and every process it owns, waiting for them.
    pub(crate) fn shutdown(&mut self) -> Option<StopOutcome> {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(Message::Quit);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        *self.stopped.lock().expect("stop outcome lock")
    }
}

impl Drop for ManagedHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temporary(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "openchamber-managed-host-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("test directory");
        directory
    }

    fn write(path: &Path, executable: bool) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        fs::write(path, "").expect("file");
        if executable {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mode");
        }
    }

    fn package(root: &Path) {
        write(&root.join("resources/bun/bun"), true);
        write(&root.join("resources/server/package.json"), false);
        write(&root.join("resources/server/server/index.js"), false);
        write(&root.join("resources/web-dist/index.html"), false);
        write(&root.join("resources/opencode-cli/opencode"), true);
    }

    #[test]
    fn discovery_takes_the_named_bundle_or_the_binary_directory() {
        let root = temporary("discover");
        package(&root);
        let canonical = fs::canonicalize(&root).expect("test fixture");
        let named = discover(Some(root.clone()), None).expect("named bundle");
        assert_eq!(named.root, canonical);
        let beside = discover(None, Some(root.join("openchamber-native"))).expect("beside");
        assert_eq!(beside.root, canonical);
        assert_eq!(discover(None, None), Err(DiscoveryError::NoCandidate));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_incomplete_named_bundle_is_not_replaced_by_another() {
        let named = temporary("incomplete");
        package(&named);
        fs::remove_file(named.join("resources/opencode-cli/opencode")).expect("test fixture");
        let beside = temporary("complete");
        package(&beside);
        let found = discover(Some(named.clone()), Some(beside.join("openchamber-native")));
        assert_eq!(
            found,
            Err(DiscoveryError::Missing(
                fs::canonicalize(&named)
                    .expect("test fixture")
                    .join("resources/opencode-cli/opencode")
            ))
        );
        // A runtime that cannot run is missing too.
        package(&named);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            named.join("resources/bun/bun"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("test fixture");
        assert!(matches!(
            discover(Some(named.clone()), None),
            Err(DiscoveryError::Missing(path)) if path.ends_with("bun/bun")
        ));
        let _ = fs::remove_dir_all(&named);
        let _ = fs::remove_dir_all(&beside);
    }

    fn value<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsStr> {
        env.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_os_str())
    }

    #[test]
    fn the_server_environment_is_the_desktops() {
        let bundle = Bundle {
            root: PathBuf::from("/opt/oc"),
        };
        let inherited = [
            ("PATH", "/usr/bin"),
            ("OPENCHAMBER_NATIVE_PASSWORD", "secret"),
            ("OPENCHAMBER_NATIVE_SERVER", "http://127.0.0.1:1"),
            ("OPENCHAMBER_UI_PASSWORD", "secret"),
            ("OPENCHAMBER_HOST", "0.0.0.0"),
            ("OPENCHAMBER_ENTERPRISE_MODE", "1"),
            ("OPENCODE_BINARY", "/home/user/opencode"),
            ("NO_PROXY", "corp.example"),
            ("ELECTRON_RUN_AS_NODE", "1"),
            ("NODE_COMPILE_CACHE", "/tmp/cache"),
        ]
        .map(|(key, value)| (OsString::from(key), OsString::from(value)));
        let env = server_environment(inherited, &bundle, Some(OsStr::new("/home/user")));
        assert_eq!(
            value(&env, "OPENCHAMBER_HOST"),
            Some(OsStr::new("127.0.0.1"))
        );
        assert_eq!(
            value(&env, "OPENCHAMBER_RUNTIME"),
            Some(OsStr::new("desktop"))
        );
        assert_eq!(
            value(&env, "OPENCHAMBER_DESKTOP_LAN_ACCESS_ACTIVE"),
            Some(OsStr::new("false"))
        );
        assert_eq!(
            value(&env, "OPENCHAMBER_DIST_DIR"),
            Some(OsStr::new("/opt/oc/resources/web-dist"))
        );
        assert_eq!(
            value(&env, "OPENCHAMBER_OPENCODE_CWD"),
            Some(OsStr::new("/home/user"))
        );
        assert_eq!(value(&env, "NO_PROXY"), Some(OsStr::new("corp.example")));
        assert_eq!(
            value(&env, "no_proxy"),
            Some(OsStr::new("localhost,127.0.0.1"))
        );
        // Policy and the user's own overrides pass through.
        assert_eq!(
            value(&env, "OPENCHAMBER_ENTERPRISE_MODE"),
            Some(OsStr::new("1"))
        );
        assert_eq!(
            value(&env, "OPENCODE_BINARY"),
            Some(OsStr::new("/home/user/opencode"))
        );
        for removed in [
            "OPENCHAMBER_NATIVE_PASSWORD",
            "OPENCHAMBER_NATIVE_SERVER",
            "OPENCHAMBER_UI_PASSWORD",
            "ELECTRON_RUN_AS_NODE",
            "NODE_COMPILE_CACHE",
        ] {
            assert_eq!(value(&env, removed), None, "{removed} stays out");
        }
        let record = value(&env, INJECTED_ENV_KEY)
            .expect("the record is written")
            .to_string_lossy()
            .into_owned();
        let names: Vec<&str> = record.split(',').collect();
        assert!(names.contains(&"OPENCHAMBER_HOST"));
        assert!(names.contains(&"no_proxy"));
        assert!(
            !names.contains(&"NO_PROXY"),
            "an unchanged value stays the user's"
        );
    }

    #[test]
    fn injected_names_extend_the_existing_record() {
        let mut env = vec![
            (OsString::from(INJECTED_ENV_KEY), OsString::from("A,B")),
            (OsString::from("A"), OsString::from("1")),
        ];
        assign_injected(
            &mut env,
            vec![
                ("A", Some("1".into())),
                ("C", Some("3".into())),
                ("D", None),
            ],
        );
        assert_eq!(value(&env, INJECTED_ENV_KEY), Some(OsStr::new("A,B,C")));
        assert_eq!(value(&env, "C"), Some(OsStr::new("3")));
        assert_eq!(value(&env, "D"), None);
    }

    #[test]
    fn the_launch_uses_the_bundled_bun_and_server_files() {
        let bundle = Bundle {
            root: PathBuf::from("/opt/oc"),
        };
        let launch = server_launch(&bundle, 4321, "n0nce".to_owned(), [], None, None);
        assert_eq!(launch.program, PathBuf::from("/opt/oc/resources/bun/bun"));
        assert_eq!(
            &launch.args[..5],
            [
                "--cwd",
                "/opt/oc/resources/server",
                "--no-env-file",
                "--no-install",
                "-e"
            ]
        );
        assert_eq!(value(&launch.env, "ELECTRON_RUN_AS_NODE"), None);
        assert_eq!(
            value(&launch.env, "OPENCHAMBER_BUNDLED_OPENCODE_CLI_DIR"),
            Some(OsStr::new("/opt/oc/resources/opencode-cli"))
        );
        let options: serde_json::Value = serde_json::from_str(
            &value(&launch.env, LAUNCH_VARIABLE)
                .expect("launch options")
                .to_string_lossy(),
        )
        .expect("test fixture");
        assert_eq!(options["port"], 4321);
        assert_eq!(options["nonce"], "n0nce");
        assert_eq!(options["entry"], "/opt/oc/resources/server/server/index.js");
        assert_eq!(
            options["builtInExtensionsDir"],
            "/opt/oc/resources/server/server/built-in-extensions"
        );
        assert_eq!(options["resourcesPath"], "/opt/oc/resources");
        assert!(options["nativeDesktopExecutable"].is_null());
        assert_eq!(value(&launch.env, "OPENCHAMBER_OPENCODE_CWD"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_colocated_bundle_identifies_the_running_executable_for_updates() {
        let exe = std::env::current_exe().expect("test executable");
        let bundle = Bundle {
            root: exe.parent().expect("test executable directory").to_owned(),
        };
        let launch = server_launch(&bundle, 4321, "fixture".to_owned(), [], None, None);
        let options: serde_json::Value = serde_json::from_str(
            value(&launch.env, LAUNCH_VARIABLE)
                .and_then(OsStr::to_str)
                .expect("launch options"),
        )
        .expect("bootstrap JSON");
        assert_eq!(options["nativeDesktopExecutable"], serde_json::json!(exe));
    }

    #[test]
    fn the_port_is_kept_while_it_is_free() {
        let directory = temporary("port");
        let store = PortStore::new(&directory);
        let first = store.choose().expect("a free port");
        assert_eq!(store.choose().expect("the same port"), first);
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, first)).expect("hold the port");
        let second = store.choose().expect("another port");
        assert_ne!(second, first, "a taken port is replaced");
        assert_eq!(store.load(), Some(second));
        drop(held);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_host_without_a_bundle_fails_and_shuts_down() {
        let mut host = ManagedHost::failed(0, HostFailure::BundleMissing);
        assert_eq!(
            *host.subscribe().borrow(),
            HostState::Failed(HostFailure::BundleMissing)
        );
        assert_eq!(host.shutdown(), None);
    }
}
