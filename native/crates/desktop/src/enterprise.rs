//! Machine-policy discovery and live updates for native-owned network
//! requests, and the projection of the connected server's own policy.
//!
//! Either source turns enterprise mode on for what native does itself: the
//! machine policy covers this computer whatever server it reaches, and a
//! server that enforces the mode is not undercut by its client. Until the
//! server's answer is known, native-owned public requests stay withheld.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui_kit::{Context, Task};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::enterprise_policy::ServerPolicy;
use serde_json::Value;

use crate::workspace::Workspace;

const MAX_POLICY_BYTES: u64 = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PolicyStatus {
    pub(crate) enabled: bool,
    pub(crate) unavailable: bool,
}

impl PolicyStatus {
    const CLOSED: Self = Self {
        enabled: true,
        unavailable: true,
    };
}

#[derive(Clone)]
pub(crate) struct PolicyGuard {
    paths: Arc<Vec<PathBuf>>,
    environment_enabled: bool,
    watcher_failed: Arc<AtomicBool>,
    rearm: Arc<AtomicBool>,
}

impl PolicyGuard {
    fn from_environment() -> Self {
        Self {
            paths: Arc::new(policy_paths(
                std::env::consts::OS,
                std::env::var("ProgramData").ok().as_deref(),
            )),
            environment_enabled: std::env::var("OPENCHAMBER_ENTERPRISE_MODE").is_ok_and(|value| {
                matches!(trim_js(&value).to_ascii_lowercase().as_str(), "1" | "true")
            }),
            watcher_failed: Arc::new(AtomicBool::new(true)),
            rearm: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Re-read immediately before a native-owned crossing, on its worker.
    /// The watcher updates presentation; it never grants request authority.
    pub(crate) fn read(&self) -> PolicyStatus {
        if self.watcher_failed.load(Ordering::Acquire) {
            return PolicyStatus::CLOSED;
        }
        resolve_policy(&self.paths, self.environment_enabled, read_file)
    }
}

fn policy_paths(platform: &str, program_data: Option<&str>) -> Vec<PathBuf> {
    match platform {
        "macos" => vec![PathBuf::from(
            "/Library/Application Support/OpenChamber/policy.json",
        )],
        "windows" => {
            let fixed = r"C:\ProgramData\OpenChamber\policy.json";
            let mut paths = vec![PathBuf::from(fixed)];
            if let Some(root) = program_data
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                let candidate = format!(
                    r"{}\OpenChamber\policy.json",
                    root.trim_end_matches(['/', '\\'])
                );
                if !candidate.eq_ignore_ascii_case(fixed) {
                    paths.push(PathBuf::from(candidate));
                }
            }
            paths
        }
        _ => vec![PathBuf::from("/etc/openchamber/policy.json")],
    }
}

fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "policy is not a file",
        ));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "policy is not a file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_POLICY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_POLICY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "policy is too large",
        ));
    }
    Ok(bytes)
}

fn resolve_policy(
    paths: &[PathBuf],
    environment_enabled: bool,
    read: impl Fn(&Path) -> std::io::Result<Vec<u8>>,
) -> PolicyStatus {
    for path in paths {
        let bytes = match read(path) {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(_) => return PolicyStatus::CLOSED,
        };
        return match parse_policy(&bytes) {
            Some(enabled) => PolicyStatus {
                enabled: enabled || environment_enabled,
                unavailable: false,
            },
            None => PolicyStatus::CLOSED,
        };
    }
    PolicyStatus {
        enabled: environment_enabled,
        unavailable: false,
    }
}

/// Validate the existing policy-file schema even for knobs this client does
/// not consume yet. A malformed knob cannot silently disable the policy.
fn parse_policy(bytes: &[u8]) -> Option<bool> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    for key in [
        "enterpriseMode",
        "allowNetworkAccess",
        "allowLocalExtensions",
        "hideBuiltinSkillCatalogs",
    ] {
        if object.get(key).is_some_and(|value| !value.is_boolean()) {
            return None;
        }
    }
    for key in ["organization", "relayUrl", "opencodeBinary"] {
        if object.get(key).is_some_and(|value| !value.is_string()) {
            return None;
        }
    }
    if let Some(entries) = object.get("allowedExtensions") {
        let entries = entries.as_array()?;
        if entries.len() > 200
            || entries
                .iter()
                .any(|value| value.as_str().is_none_or(|value| trim_js(value).is_empty()))
        {
            return None;
        }
    }
    if let Some(jev) = object.get("jev") {
        let jev = jev.as_object()?;
        for key in ["url", "model", "apiKey"] {
            if jev.get(key).is_some_and(|value| !value.is_string()) {
                return None;
            }
        }
        let has = |key| {
            jev.get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !trim_js(value).is_empty())
        };
        if !has("url") && (has("model") || has("apiKey")) {
            return None;
        }
    }
    Some(
        object
            .get("enterpriseMode")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(character,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

pub(crate) struct PolicyMonitor {
    pub(crate) guard: PolicyGuard,
    pub(crate) status: PolicyStatus,
    watcher: Option<RecommendedWatcher>,
    watched: Vec<PathBuf>,
    task: Option<Task<()>>,
    /// The runtime `server` belongs to.
    server_runtime: Option<RuntimeId>,
    /// The server's policy; `None` until a read for this runtime settled.
    /// A server from before the route reports no policy.
    pub(crate) server: Option<ServerPolicy>,
    server_read: Option<Task<()>>,
}

impl Default for PolicyMonitor {
    fn default() -> Self {
        Self {
            guard: PolicyGuard::from_environment(),
            status: PolicyStatus::CLOSED,
            watcher: None,
            watched: Vec::new(),
            task: None,
            server_runtime: None,
            server: None,
            server_read: None,
        }
    }
}

impl PolicyMonitor {
    /// Whether enterprise mode is on for native's own surfaces: the machine
    /// policy (closed while unreadable) or the connected server says so.
    pub(crate) fn enterprise_mode(&self) -> bool {
        self.status.enabled
            || self
                .server
                .as_ref()
                .is_some_and(|policy| policy.enterprise_mode)
    }

    /// Whether native-owned public requests are withheld: in enterprise mode,
    /// and while a connected server's policy is still unknown.
    pub(crate) fn withholds_public_requests(&self, connected: bool) -> bool {
        self.enterprise_mode() || (connected && self.server.is_none())
    }

    /// Adopts the answer for `runtime`, if one arrived (`None` for a route
    /// the server predates). Without an answer, what was known for the same
    /// runtime stays; another runtime starts unknown. Returns whether the
    /// projection changed.
    fn apply_server(&mut self, runtime: &RuntimeId, answer: Option<Option<ServerPolicy>>) -> bool {
        let before = (self.server_runtime.clone(), self.server.clone());
        if self.server_runtime.as_ref() != Some(runtime) {
            self.server_runtime = Some(runtime.clone());
            self.server = None;
        }
        if let Some(policy) = answer {
            self.server = Some(policy.unwrap_or_default());
        }
        before != (self.server_runtime.clone(), self.server.clone())
    }

    fn watch_parents(&mut self) -> bool {
        let Some(watcher) = self.watcher.as_mut() else {
            return false;
        };
        if self.guard.rearm.swap(false, Ordering::AcqRel) {
            for path in self.watched.drain(..) {
                let _ = watcher.unwatch(&path);
            }
        }
        let mut needed = Vec::new();
        for path in self.guard.paths.iter() {
            let Some(parent) = path.ancestors().skip(1).find(|parent| parent.is_dir()) else {
                return false;
            };
            if !needed.contains(&parent.to_path_buf()) {
                needed.push(parent.to_path_buf());
            }
        }
        // Watch the replacement parents before removing old coverage.
        for path in &needed {
            if !self.watched.contains(path)
                && watcher.watch(path, RecursiveMode::NonRecursive).is_err()
            {
                return false;
            }
        }
        for path in &self.watched {
            if !needed.contains(path) {
                let _ = watcher.unwatch(path);
            }
        }
        self.watched = needed;
        true
    }
}

fn event_relevance(event: &notify::Event, paths: &[PathBuf]) -> (bool, bool) {
    if event.need_rescan() {
        return (true, true);
    }
    let relevant = !matches!(event.kind, notify::EventKind::Access(_))
        && event.paths.iter().any(|changed| {
            paths
                .iter()
                .any(|path| path == changed || path.starts_with(changed))
        });
    let rearm = relevant
        && matches!(
            event.kind,
            notify::EventKind::Remove(_)
                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
    (relevant, rearm)
}

impl Workspace {
    /// Reads the connected server's policy, once per connection.
    pub(crate) fn load_server_policy(&mut self, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        if self.enterprise.apply_server(&runtime, None) {
            self.provider_logos.clear_remote();
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let read = connection
            .network
            .run(async move { client.enterprise_policy().await });
        self.enterprise.server_read = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                let answer = match result {
                    Some(Ok(policy)) => Some(policy),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the server's enterprise policy could not be read");
                        None
                    }
                    None => None,
                };
                if this.enterprise.apply_server(&runtime, answer) {
                    this.provider_logos.clear_remote();
                    cx.notify();
                }
                if let Some(policy) = &this.enterprise.server {
                    crate::diagnostics::mark(
                        crate::diagnostics::Marker::ServerPolicyRead,
                        None,
                        Some(usize::from(policy.enterprise_mode)),
                    );
                }
            });
        }));
    }

    pub(crate) fn start_policy_monitor(&mut self, cx: &mut Context<Self>) {
        let (events, mut updates) = tokio::sync::mpsc::channel(1);
        let guard = self.enterprise.guard.clone();
        let paths = guard.paths.clone();
        let failed = guard.watcher_failed.clone();
        let rearm = guard.rearm.clone();
        self.enterprise.watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let relevant = match event {
                    Ok(event) => {
                        let (relevant, needs_rearm) = event_relevance(&event, &paths);
                        if needs_rearm {
                            rearm.store(true, Ordering::Release);
                        }
                        relevant
                    }
                    Err(_) => {
                        failed.store(true, Ordering::Release);
                        rearm.store(true, Ordering::Release);
                        true
                    }
                };
                if relevant {
                    let _ = events.try_send(());
                }
            })
            .ok();
        let watching = self.enterprise.watch_parents();
        self.enterprise
            .guard
            .watcher_failed
            .store(!watching, Ordering::Release);
        self.enterprise.task = Some(cx.spawn(async move |this, cx| {
            loop {
                if this
                    .update(cx, |this, _| {
                        let watching = this.enterprise.watch_parents();
                        this.enterprise
                            .guard
                            .watcher_failed
                            .store(!watching, Ordering::Release);
                    })
                    .is_err()
                {
                    break;
                }
                let current = guard.clone();
                let status = cx
                    .background_executor()
                    .spawn(async move { current.read() })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        let status = if this.enterprise.guard.watcher_failed.load(Ordering::Acquire)
                        {
                            PolicyStatus::CLOSED
                        } else {
                            status
                        };
                        crate::diagnostics::mark(
                            crate::diagnostics::Marker::PolicyChecked,
                            None,
                            Some(usize::from(status.enabled)),
                        );
                        if this.enterprise.status != status {
                            this.enterprise.status = status;
                            this.provider_logos.clear_remote();
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                if updates.recv().await.is_none() {
                    break;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_authority_cannot_be_hidden_by_windows_environment_redirection() {
        let paths = policy_paths("windows", Some(r"D:\UserData"));
        assert_eq!(
            paths[0],
            PathBuf::from(r"C:\ProgramData\OpenChamber\policy.json")
        );
        let status = resolve_policy(&paths, false, |path| {
            if path == paths[0] {
                Ok(br#"{"enterpriseMode":true}"#.to_vec())
            } else {
                Ok(br#"{"enterpriseMode":false}"#.to_vec())
            }
        });
        assert!(status.enabled);
    }

    #[test]
    fn malformed_or_unreadable_policy_is_closed_even_when_environment_is_off() {
        let paths = policy_paths("linux", None);
        for input in [
            b"[]".as_slice(),
            b"null",
            b"broken",
            br#"{"enterpriseMode":null}"#,
            br#"{"allowedExtensions":[""]}"#,
            br#"{"jev":{"model":"m"}}"#,
            br#"{"hideBuiltinSkillCatalogs":"false"}"#,
        ] {
            assert_eq!(
                resolve_policy(&paths, false, |_| Ok(input.to_vec())),
                PolicyStatus::CLOSED
            );
        }
        assert_eq!(
            resolve_policy(&paths, false, |_| Err(
                std::io::ErrorKind::PermissionDenied.into()
            )),
            PolicyStatus::CLOSED
        );
    }

    #[test]
    fn either_source_can_enable_and_unknown_fields_follow_the_existing_schema() {
        let paths = policy_paths("linux", None);
        assert!(
            resolve_policy(
                &paths,
                true,
                |_| Ok(br#"{"enterpriseMode":false}"#.to_vec())
            )
            .enabled
        );
        assert!(!resolve_policy(&paths, false, |_| Ok(br#"{"futureKnob":true}"#.to_vec())).enabled);
        assert_eq!(
            parse_policy(b"\xef\xbb\xbf{\"enterpriseMode\":true}"),
            Some(true)
        );
        assert_eq!(parse_policy(b"\xef\xbb\xbf\xef\xbb\xbf{}"), None);
    }

    #[test]
    fn a_pathless_rescan_rearms_policy_watch_coverage() {
        let event =
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert_eq!(
            event_relevance(&event, &policy_paths("linux", None)),
            (true, true)
        );
    }

    #[test]
    fn server_policy_stays_unknown_until_answered_and_either_source_restricts() {
        let mut monitor = PolicyMonitor {
            status: PolicyStatus {
                enabled: false,
                unavailable: false,
            },
            ..PolicyMonitor::default()
        };
        let (one, two) = (RuntimeId::from("one"), RuntimeId::from("two"));
        assert!(!monitor.withholds_public_requests(false), "no server, no wait");
        assert!(
            monitor.withholds_public_requests(true),
            "an unanswered server policy withholds public requests"
        );
        assert!(monitor.apply_server(&one, Some(None)), "a server from before the route");
        assert!(!monitor.enterprise_mode() && !monitor.withholds_public_requests(true));
        let restricted = ServerPolicy {
            enterprise_mode: true,
            organization: Some("Acme".to_owned()),
            ..ServerPolicy::default()
        };
        assert!(monitor.apply_server(&one, Some(Some(restricted))));
        assert!(monitor.enterprise_mode(), "the server's mode applies to native surfaces");
        assert!(!monitor.apply_server(&one, None), "a failed read keeps what was known");
        assert!(monitor.enterprise_mode());
        assert!(monitor.apply_server(&two, None), "another runtime starts unknown");
        assert!(monitor.server.is_none() && !monitor.enterprise_mode());
        assert!(monitor.withholds_public_requests(true));
        assert!(monitor.apply_server(&two, Some(Some(ServerPolicy::default()))));
        monitor.status = PolicyStatus::CLOSED;
        assert!(
            monitor.enterprise_mode(),
            "an unreadable machine policy restricts whatever the server says"
        );
    }

    #[gpui_kit::test]
    fn enterprise_mode_hides_the_integrations_catalog(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt;
        use gpui_kit::{AppContext, ElementId};
        let (window, workspace) = crate::workspace_tests::open(cx);
        for (status, shown) in [
            (
                PolicyStatus {
                    enabled: false,
                    unavailable: false,
                },
                true,
            ),
            (PolicyStatus::CLOSED, false),
        ] {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, _| workspace.enterprise.status = status);
                window.render_frame(cx);
                assert_eq!(
                    window
                        .try_find(ElementId::Name("integrations".into()))
                        .is_some(),
                    shown
                );
            })
            .expect("window");
        }
    }

    #[gpui_kit::test]
    fn policy_creation_replacement_corruption_and_removal_apply_live(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use std::time::{Duration, Instant};
        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let directory = fixture
            .journal()
            .parent()
            .expect("profile directory")
            .to_path_buf();
        std::fs::create_dir_all(&directory).expect("private directory");
        let watched_directory = directory.join("managed");
        let file = watched_directory.join("policy.json");
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.enterprise = PolicyMonitor::default();
                workspace.enterprise.guard.paths = Arc::new(vec![file.clone()]);
                workspace.enterprise.guard.environment_enabled = false;
                workspace.start_policy_monitor(cx);
            })
        });
        let wait = |cx: &mut gpui_kit::TestAppContext, wanted: PolicyStatus| {
            let start = Instant::now();
            while !cx.read(|cx| workspace.read(cx).enterprise.status == wanted) {
                cx.run_until_parked();
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "policy change must be observed"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let open = PolicyStatus {
            enabled: false,
            unavailable: false,
        };
        wait(cx, open);
        std::fs::create_dir(&watched_directory).expect("policy directory created after watch");
        let temporary = watched_directory.join("replacement.json");
        std::fs::write(&temporary, br#"{"enterpriseMode":true}"#).expect("valid policy");
        std::fs::rename(&temporary, &file).expect("atomic policy replacement");
        wait(
            cx,
            PolicyStatus {
                enabled: true,
                unavailable: false,
            },
        );
        std::fs::rename(&watched_directory, directory.join("retired-managed"))
            .expect("watched directory moved");
        std::fs::create_dir(&watched_directory).expect("replacement directory");
        std::fs::write(&file, b"{}").expect("replacement policy");
        wait(cx, open);
        std::fs::write(&file, b"invalid").expect("corrupted policy");
        wait(cx, PolicyStatus::CLOSED);
        std::fs::write(&file, br#"{"enterpriseMode":true}"#).expect("replacement policy updated");
        wait(
            cx,
            PolicyStatus {
                enabled: true,
                unavailable: false,
            },
        );
        std::fs::remove_file(&file).expect("policy removed");
        wait(cx, open);
    }
}
