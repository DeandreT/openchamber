//! Real Windows subprocess fixtures for the unchanged supervisor state machine.
use super::*;
use crate::managed_host_lifecycle::HostFailure;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

const PORT: u16 = 45_678;

struct Harness {
    commands: Sender<Message>,
    states: watch::Receiver<HostState>,
    stopped: Arc<Mutex<Option<StopOutcome>>>,
    thread: Option<thread::JoinHandle<()>>,
    directory: PathBuf,
}

impl Harness {
    fn start(case: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("oc-win-supervisor-{case}-{}", std::process::id()));
        std::fs::create_dir(&directory).expect("unique fixture directory");
        let mut env: Vec<_> = std::env::vars_os()
            .filter(|(key, _)| key != "OC_HOST_CASE" && key != "OC_HOST_WORK")
            .collect();
        env.push(("OC_HOST_CASE".into(), case.into()));
        env.push(("OC_HOST_WORK".into(), directory.as_os_str().into()));
        let launch = Launch {
            program: std::env::current_exe().expect("test executable"),
            args: [
                "--exact",
                "managed_host_supervisor::windows_tests::fixture",
                "--ignored",
                "--nocapture",
            ]
            .map(OsString::from)
            .to_vec(),
            env,
            port: PORT,
            nonce: "windows-fixture".to_owned(),
            log: None,
        };
        let (state, states) = watch::channel(HostState::Starting);
        let (commands, messages) = mpsc::channel();
        let stopped = Arc::new(Mutex::new(None));
        let supervisor = Supervisor::new(
            launch,
            Timeouts {
                ready: Duration::from_secs(10),
                stop: Duration::from_millis(300),
                reap: Duration::from_secs(5),
            },
            state,
            messages,
            commands.clone(),
            stopped.clone(),
        );
        let thread = Some(thread::spawn(move || supervisor.run()));
        Self {
            commands,
            states,
            stopped,
            thread,
            directory,
        }
    }

    fn wait(&mut self, wanted: HostState) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while *self.states.borrow_and_update() != wanted {
            assert!(
                Instant::now() < deadline,
                "expected {wanted:?}, got {:?}",
                *self.states.borrow()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn quit(&mut self) {
        let _ = self.commands.send(Message::Quit);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("supervisor exits");
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.quit();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn windows_stdin_shutdown_is_graceful() {
    assert_eq!(Timeouts::default().stop, Duration::from_secs(35));
    let mut host = Harness::start("graceful");
    host.wait(HostState::Ready { port: PORT });
    host.quit();
    assert_eq!(
        *host.stopped.lock().expect("outcome"),
        Some(StopOutcome::Graceful)
    );
    assert_eq!(*host.states.borrow(), HostState::Stopped);
}

#[test]
fn windows_root_exit_with_live_descendants_requires_forced_job_cleanup() {
    let mut host = Harness::start("descendant");
    host.wait(HostState::Ready { port: PORT });
    host.quit();
    assert_eq!(
        *host.stopped.lock().expect("outcome"),
        Some(StopOutcome::Forced)
    );
    assert_eq!(*host.states.borrow(), HostState::Stopped);
}

#[test]
fn windows_startup_exit_fails_and_cleans_up() {
    let mut host = Harness::start("startup-exit");
    host.wait(HostState::Failed(HostFailure::StartupExited));
    host.quit();
}

#[test]
fn windows_crash_recovers_under_a_fresh_job() {
    let mut host = Harness::start("crash-once");
    host.wait(HostState::Ready { port: PORT });
    std::fs::write(host.directory.join("crash-now"), "").expect("release crash");
    host.wait(HostState::Recovering { attempt: 1 });
    host.wait(HostState::Ready { port: PORT });
    host.quit();
    assert_eq!(
        *host.stopped.lock().expect("outcome"),
        Some(StopOutcome::Graceful)
    );
}

#[test]
#[ignore]
fn fixture() {
    let Ok(case) = std::env::var("OC_HOST_CASE") else {
        return;
    };
    if case == "leaf" {
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    let work = PathBuf::from(std::env::var_os("OC_HOST_WORK").expect("fixture directory"));
    if case == "descendant"
        || case == "startup-exit"
        || case == "crash-once" && !work.join("crashed").exists()
    {
        let _child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "managed_host_supervisor::windows_tests::fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("OC_HOST_CASE", "leaf")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .expect("job descendant");
    }
    if case == "startup-exit" {
        std::process::exit(7);
    }
    println!("windows-fixture ready {PORT}");
    if case == "crash-once" && !work.join("crashed").exists() {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !work.join("crash-now").is_file() {
            assert!(Instant::now() < deadline, "crash checkpoint");
            thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(work.join("crashed"), "").expect("crash marker");
        std::process::exit(9);
    }
    io::stdin().read_to_end(&mut Vec::new()).expect("stdin EOF");
}
