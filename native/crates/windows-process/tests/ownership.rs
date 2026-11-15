#![cfg(windows)]
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use openchamber_windows_process::{Spawned, spawn};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

const WAIT: Duration = Duration::from_secs(10);

fn arguments() -> Vec<OsString> {
    ["--exact", "fixture", "--ignored", "--nocapture"]
        .map(OsString::from)
        .to_vec()
}

fn environment(mode: &str, directory: &Path) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| key != "OC_FIXTURE_MODE" && key != "OC_FIXTURE_DIRECTORY")
        .collect();
    env.push(("OC_FIXTURE_MODE".into(), mode.into()));
    env.push(("OC_FIXTURE_DIRECTORY".into(), directory.as_os_str().into()));
    env
}

fn launch(mode: &str, directory: &Path) -> Spawned {
    spawn(
        &std::env::current_exe().expect("test executable"),
        &arguments(),
        &environment(mode, directory),
    )
    .expect("managed fixture starts")
}

struct Directory(PathBuf);
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Directory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("oc-winjob-{name}-{}", std::process::id()));
        fs::create_dir(&path).expect("unique fixture directory");
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "fixture did not reach its checkpoint"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Test-only observation retains a synchronize handle before cleanup. No operation
// terminates a PID-opened handle; termination uses the private job exclusively.
fn observe(path: &Path) -> OwnedHandle {
    wait_until(|| path.is_file());
    let pid: u32 = fs::read_to_string(path)
        .expect("fixture PID")
        .parse()
        .expect("numeric PID");
    // SAFETY: OpenProcess returns a fresh handle to the live synthetic fixture.
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("observe fixture");
    // SAFETY: the successful OpenProcess handle is owned exactly once.
    unsafe { OwnedHandle::from_raw_handle(raw.0) }
}

fn exited(process: &OwnedHandle) -> bool {
    // SAFETY: this retained synchronize handle stays live during the call.
    let result = unsafe { WaitForSingleObject(HANDLE(process.as_raw_handle()), 0) };
    assert!(result == WAIT_OBJECT_0 || result == WAIT_TIMEOUT);
    result == WAIT_OBJECT_0
}

#[test]
fn stdin_shutdown_waits_for_server_and_job_exhaustion() {
    let directory = Directory::new("graceful");
    let started = launch("graceful", &directory.0);
    assert!(started.process.identity().creation_time > 0);
    let original = started.process.identity();
    drop(started.stdin);
    assert_eq!(started.process.wait().expect("server exit"), 0);
    assert!(started.job.wait_empty(WAIT).expect("job accounting"));
    assert_eq!(
        started.process.identity(),
        original,
        "identity survives process exit"
    );
}

#[test]
fn argument_quoting_and_complete_environment_survive_direct_creation() {
    let directory = Directory::new("arguments");
    let expected = OsString::from("spaces \"quoted\" trailing\\");
    let mut args = arguments();
    args.extend(["--skip".into(), expected.clone()]);
    let env = vec![
        ("OC_FIXTURE_MODE".into(), "echo".into()),
        (
            "OC_FIXTURE_DIRECTORY".into(),
            directory.0.as_os_str().into(),
        ),
        (
            "oc_fixture_value".into(),
            "superseded case-insensitive value".into(),
        ),
        (
            "OC_FIXTURE_VALUE".into(),
            "unicode \u{3bb} = \"value\"".into(),
        ),
    ];
    let started = spawn(
        &std::env::current_exe().expect("test executable"),
        &args,
        &env,
    )
    .expect("direct launch");
    assert_eq!(started.process.wait().expect("echo fixture exit"), 0);
    assert_eq!(
        fs::read_to_string(directory.0.join("argument")).expect("argument"),
        expected.to_str().expect("UTF-8 fixture")
    );
    assert_eq!(
        fs::read_to_string(directory.0.join("environment")).expect("environment"),
        "unicode \u{3bb} = \"value\""
    );
    assert!(started.job.wait_empty(WAIT).expect("job accounting"));
}

#[test]
fn descendants_cannot_break_away_from_the_private_job() {
    let directory = Directory::new("breakaway");
    let started = launch("breakaway", &directory.0);
    assert_eq!(started.process.wait().expect("fixture exit"), 0);
    assert!(started.job.wait_empty(WAIT).expect("job accounting"));
}

#[test]
fn descendants_outliving_root_remain_owned_and_unrelated_process_survives() {
    let directory = Directory::new("outlive");
    let mut unrelated = ChildGuard(
        Command::new(std::env::current_exe().expect("test exe"))
            .args(arguments())
            .envs(environment("leaf", &directory.0))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .expect("unrelated fixture"),
    );
    let started = launch("tree-exit", &directory.0);
    let descendant = observe(&directory.0.join("descendant.pid"));
    assert_eq!(started.process.wait().expect("root exit"), 23);
    assert!(!exited(&descendant), "descendant outlives its root");
    assert!(!started.job.is_empty().expect("job accounting"));
    started.job.terminate().expect("force private job");
    assert!(started.job.wait_empty(WAIT).expect("empty job"));
    wait_until(|| exited(&descendant));
    assert!(unrelated.0.try_wait().expect("unrelated state").is_none());
}

#[test]
fn owner_crash_closes_job_and_kills_descendants() {
    let directory = Directory::new("owner-crash");
    let mut owner = ChildGuard(
        Command::new(std::env::current_exe().expect("test exe"))
            .args(arguments())
            .envs(environment("crash-owner", &directory.0))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .expect("owner fixture"),
    );
    let descendant = observe(&directory.0.join("descendant.pid"));
    fs::write(directory.0.join("crash-now"), "").expect("release crash checkpoint");
    assert_eq!(owner.0.wait().expect("owner exit").code(), Some(29));
    wait_until(|| exited(&descendant));
}

#[test]
fn startup_failure_releases_job_and_server_crash_leaves_cleanup_authority() {
    let directory = Directory::new("startup");
    assert!(spawn(&directory.0.join("absent.exe"), &[], &[]).is_err());
    let started = launch("tree-exit", &directory.0);
    let descendant = observe(&directory.0.join("descendant.pid"));
    assert_eq!(started.process.wait().expect("startup root failed"), 23);
    drop(started);
    wait_until(|| exited(&descendant));
}

/// Runs in isolated subprocesses only. It never sees application settings or data.
#[test]
#[ignore]
fn fixture() {
    let Ok(mode) = std::env::var("OC_FIXTURE_MODE") else {
        return;
    };
    let directory =
        PathBuf::from(std::env::var_os("OC_FIXTURE_DIRECTORY").expect("fixture directory"));
    match mode.as_str() {
        "breakaway" => {
            let result = Command::new(std::env::current_exe().expect("fixture exe"))
                .args(arguments())
                .env("OC_FIXTURE_MODE", "leaf")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x08000000 | 0x01000000)
                .spawn();
            if let Ok(mut escaped) = result {
                let _ = escaped.kill();
                let _ = escaped.wait();
                panic!("private job incorrectly allowed breakaway");
            }
        }
        "echo" => {
            assert!(
                std::env::var_os("PATH").is_none(),
                "no implicit parent environment"
            );
            fs::write(
                directory.join("argument"),
                std::env::args().next_back().expect("quoted argument"),
            )
            .expect("argument evidence");
            fs::write(
                directory.join("environment"),
                std::env::var("OC_FIXTURE_VALUE").expect("explicit value"),
            )
            .expect("environment evidence");
        }
        "leaf" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        "graceful" => {
            io::stdin().read_to_end(&mut Vec::new()).expect("stdin EOF");
        }
        "tree-exit" => {
            let child = Command::new(std::env::current_exe().expect("fixture exe"))
                .args(arguments())
                .env("OC_FIXTURE_MODE", "leaf")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x08000000)
                .spawn()
                .expect("descendant");
            fs::write(directory.join("descendant.partial"), child.id().to_string())
                .expect("descendant PID");
            fs::rename(
                directory.join("descendant.partial"),
                directory.join("descendant.pid"),
            )
            .expect("publish complete descendant PID");
            std::process::exit(23);
        }
        "crash-owner" => {
            let _started = launch("tree-exit", &directory);
            wait_until(|| directory.join("crash-now").is_file());
            std::process::exit(29);
        }
        _ => panic!("unknown synthetic fixture mode"),
    }
}
