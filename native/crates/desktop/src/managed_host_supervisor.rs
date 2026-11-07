//! The thread that owns the managed server process.
//!
//! One supervisor runs per launch. It starts the server in its own process
//! group, records the server's identity, reads its readiness line, restarts
//! it after a crash as [`Lifecycle`] allows and, on quit, stops it the way the
//! desktop stops its in-process backend (`packages/electron/server-shutdown.mjs`):
//! ask once, wait up to [`Timeouts::stop`], and only then force the processes
//! it owns. Closing the server's stdin is the request; the server's bootstrap
//! treats end of input as quit, so a native client that dies without running
//! its own shutdown still stops its server.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::managed_host_lifecycle::{Event, HostState, Lifecycle, Next};
use crate::owned_process::{self, OwnedProcess, ProcessIdentity, Signal, Target};

/// How long an exit waits for the output that preceded it.
const OUTPUT_DRAIN: Duration = Duration::from_secs(1);

/// What starts one server.
#[derive(Clone, Debug)]
pub(crate) struct Launch {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    /// The complete environment; nothing else is inherited.
    pub(crate) env: Vec<(OsString, OsString)>,
    /// The port the server was told to listen on.
    pub(crate) port: u16,
    /// Marks the readiness line among the server's own output.
    pub(crate) nonce: String,
    /// Where the server's output goes; discarded when absent.
    pub(crate) log: Option<PathBuf>,
}

/// The supervisor's deadlines.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeouts {
    pub(crate) ready: Duration,
    /// The desktop's 35 s: the terminal runtime's 20 s grace plus the rest of
    /// the backend's cleanup.
    pub(crate) stop: Duration,
    /// Between the forced stop's terminate and kill, as the desktop's
    /// detached OpenCode killer waits.
    pub(crate) kill_grace: Duration,
    /// How long a forced stop waits for the server to be reaped.
    pub(crate) reap: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            ready: Duration::from_secs(60),
            stop: Duration::from_secs(35),
            kill_grace: Duration::from_millis(100),
            reap: Duration::from_secs(5),
        }
    }
}

/// What reaches the supervisor thread.
#[derive(Debug)]
pub(crate) enum Message {
    Ready { attempt: u64, port: u16 },
    Exited { attempt: u64 },
    Quit,
}

/// How a stop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// The server exited on its own after the request.
    Graceful,
    /// The server was forced after the deadline.
    Forced,
}

/// One running server.
struct Running {
    attempt: u64,
    identity: ProcessIdentity,
    stdin: Option<ChildStdin>,
    /// Set once the waiter reaped the server; its PID may be reused after.
    reaped: Arc<Mutex<bool>>,
}

pub(crate) struct Supervisor {
    launch: Launch,
    timeouts: Timeouts,
    lifecycle: Lifecycle,
    state: watch::Sender<HostState>,
    messages: Receiver<Message>,
    sender: Sender<Message>,
    attempt: u64,
    /// The last stop's outcome, for the caller and tests.
    stopped: Arc<Mutex<Option<StopOutcome>>>,
}

impl Supervisor {
    pub(crate) fn new(
        launch: Launch,
        timeouts: Timeouts,
        state: watch::Sender<HostState>,
        messages: Receiver<Message>,
        sender: Sender<Message>,
        stopped: Arc<Mutex<Option<StopOutcome>>>,
    ) -> Self {
        let (lifecycle, _) = Lifecycle::starting();
        Self {
            launch,
            timeouts,
            lifecycle,
            state,
            messages,
            sender,
            attempt: 0,
            stopped,
        }
    }

    /// Runs until quit and the stop of every owned process.
    pub(crate) fn run(mut self) {
        let mut running: Option<Running> = None;
        let mut spawn_at = Some(Instant::now());
        let mut ready_by: Option<Instant> = None;
        self.publish();
        loop {
            if let Some(at) = spawn_at
                && Instant::now() >= at
                && running.is_none()
            {
                spawn_at = None;
                match self.spawn() {
                    Ok(started) => {
                        ready_by = Some(Instant::now() + self.timeouts.ready);
                        running = Some(started);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the managed server could not start");
                        self.apply(Event::SpawnFailed, &mut running, &mut spawn_at);
                    }
                }
                continue;
            }
            let wake = [spawn_at, ready_by.filter(|_| self.lifecycle.awaits_ready())]
                .into_iter()
                .flatten()
                .min();
            let message = match wake {
                Some(at) => match self
                    .messages
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => Some(Message::Quit),
                },
                None => Some(self.messages.recv().unwrap_or(Message::Quit)),
            };
            let current = running.as_ref().map(|running| running.attempt);
            match message {
                None => {
                    if ready_by.is_some_and(|at| Instant::now() >= at)
                        && self.lifecycle.awaits_ready()
                    {
                        ready_by = None;
                        tracing::warn!("the managed server did not become ready in time");
                        self.apply(Event::StartupTimedOut, &mut running, &mut spawn_at);
                    }
                }
                Some(Message::Ready { attempt, port }) if Some(attempt) == current => {
                    if port == self.launch.port {
                        ready_by = None;
                        self.apply(Event::Ready(port), &mut running, &mut spawn_at);
                    } else {
                        tracing::warn!(port, "the managed server listens on another port");
                    }
                }
                Some(Message::Exited { attempt }) if Some(attempt) == current => {
                    running = None;
                    ready_by = None;
                    tracing::warn!("the managed server exited");
                    self.apply(Event::Exited, &mut running, &mut spawn_at);
                }
                Some(Message::Quit) => {
                    self.apply(Event::QuitRequested, &mut running, &mut spawn_at);
                    self.apply(Event::Stopped, &mut running, &mut spawn_at);
                    return;
                }
                // Lines and exits of an attempt that was already replaced.
                Some(_) => {}
            }
        }
    }

    fn apply(
        &mut self,
        event: Event,
        running: &mut Option<Running>,
        spawn_at: &mut Option<Instant>,
    ) {
        let Next {
            stop_child,
            spawn_after,
        } = self.lifecycle.apply(event, Instant::now());
        self.publish();
        if stop_child && let Some(server) = running.take() {
            let (outcome, quit) = stop(server, &self.messages, self.timeouts);
            *self.stopped.lock().expect("stop outcome lock") = Some(outcome);
            // A quit that arrived during the wait is handled next.
            if quit && event != Event::QuitRequested {
                let _ = self.sender.send(Message::Quit);
            }
        }
        *spawn_at = spawn_after.map(|delay| Instant::now() + delay);
    }

    fn publish(&self) {
        self.state.send_replace(self.lifecycle.state());
    }

    fn spawn(&mut self) -> io::Result<Running> {
        self.attempt += 1;
        let attempt = self.attempt;
        let mut child = Command::new(&self.launch.program)
            .args(&self.launch.args)
            .env_clear()
            .envs(self.launch.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so a forced stop reaches the processes it runs
            // without a detached group of their own.
            .process_group(0)
            .spawn()?;
        // Unreaped, the PID cannot name another process yet.
        let Some(identity) = owned_process::identify(child.id()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("the server's identity could not be read"));
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let log = self.launch.log.as_deref().and_then(open_log);
        // The reader says when the output ended, so an exit is reported only
        // after a readiness line written just before it.
        let (output_done, output_ended) = mpsc::channel::<()>();
        if let Some(stdout) = stdout {
            let sender = self.sender.clone();
            let nonce = self.launch.nonce.clone();
            let log = log.as_ref().and_then(|file| file.try_clone().ok());
            thread::Builder::new()
                .name("managed-host-stdout".to_owned())
                .spawn(move || {
                    read_output(stdout, &nonce, log, attempt, &sender);
                    drop(output_done);
                })?;
        }
        if let Some(mut stderr) = stderr {
            let mut log = log;
            thread::Builder::new()
                .name("managed-host-stderr".to_owned())
                .spawn(move || match log.as_mut() {
                    Some(file) => {
                        let _ = io::copy(&mut stderr, file);
                    }
                    None => {
                        let _ = io::copy(&mut stderr, &mut io::sink());
                    }
                })?;
        }
        let reaped = Arc::new(Mutex::new(false));
        let waiter_reaped = reaped.clone();
        let sender = self.sender.clone();
        thread::Builder::new()
            .name("managed-host-wait".to_owned())
            .spawn(move || {
                let status = child.wait();
                *waiter_reaped.lock().expect("reaped lock") = true;
                tracing::info!(?status, "the managed server was reaped");
                // Bounded: a descendant that kept the pipe open holds the
                // output past the server's exit.
                let _ = output_ended.recv_timeout(OUTPUT_DRAIN);
                let _ = sender.send(Message::Exited { attempt });
            })?;
        tracing::info!(pid = identity.pid, "the managed server started");
        Ok(Running {
            attempt,
            identity,
            stdin,
            reaped,
        })
    }
}

fn open_log(path: &Path) -> Option<File> {
    File::options().create(true).append(true).open(path).ok()
}

/// The port of a readiness line `<nonce> ready <port>`.
pub(crate) fn parse_ready(line: &str, nonce: &str) -> Option<u16> {
    let rest = line.trim().strip_prefix(nonce)?.strip_prefix(" ready ")?;
    rest.parse().ok()
}

/// Drains the server's stdout, so it never blocks on a full pipe, and turns
/// the readiness line into a message. Everything else goes to the log.
fn read_output(
    stdout: impl io::Read,
    nonce: &str,
    mut log: Option<File>,
    attempt: u64,
    sender: &Sender<Message>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(port) = parse_ready(&text, nonce) {
            let _ = sender.send(Message::Ready { attempt, port });
            continue;
        }
        if let Some(file) = log.as_mut() {
            let _ = file.write_all(&line);
        }
    }
}

/// Stops one server: request, bounded wait, then force. Also says whether a
/// quit arrived meanwhile.
fn stop(
    mut server: Running,
    messages: &Receiver<Message>,
    timeouts: Timeouts,
) -> (StopOutcome, bool) {
    // Taken while the server still runs: afterwards its detached children
    // have a new parent and can no longer be traced to it.
    let owned = owned_process::descendants(server.identity);
    drop(server.stdin.take());
    let mut quit = false;
    if wait_exit(messages, server.attempt, timeouts.stop, &mut quit) {
        return (StopOutcome::Graceful, quit);
    }
    tracing::warn!("the managed server did not stop in time; forcing it");
    force(&server, &owned, timeouts.kill_grace);
    if !wait_exit(messages, server.attempt, timeouts.reap, &mut quit) {
        tracing::error!("the managed server could not be stopped");
    }
    (StopOutcome::Forced, quit)
}

/// Terminates the groups the server's descendants lead, then kills them and
/// every remaining descendant, then the server's own group. Each signal
/// checks the target's identity first.
fn force(server: &Running, owned: &[OwnedProcess], grace: Duration) {
    let leaders: Vec<&OwnedProcess> = owned
        .iter()
        .filter(|process| process.leads_group())
        .collect();
    for leader in &leaders {
        let _ = owned_process::send(leader.identity, Target::Group, Signal::Terminate);
    }
    thread::sleep(grace);
    for leader in &leaders {
        let _ = owned_process::send(leader.identity, Target::Group, Signal::Kill);
    }
    for process in owned {
        let _ = owned_process::send(process.identity, Target::Process, Signal::Kill);
    }
    let reaped = server.reaped.lock().expect("reaped lock");
    if !*reaped {
        let _ = owned_process::send(server.identity, Target::Group, Signal::Kill);
        let _ = owned_process::send(server.identity, Target::Process, Signal::Kill);
    }
}

/// Waits for the exit of `attempt`. Other messages belong to replaced
/// attempts, except a quit, which is noted.
fn wait_exit(
    messages: &Receiver<Message>,
    attempt: u64,
    timeout: Duration,
    quit: &mut bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match messages.recv_timeout(left) {
            Ok(Message::Exited { attempt: exited }) if exited == attempt => return true,
            Ok(Message::Quit) => *quit = true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::managed_host_lifecycle::HostFailure;

    const PORT: u16 = 45_678;
    const NONCE: &str = "test-nonce";

    struct Harness {
        commands: Sender<Message>,
        states: watch::Receiver<HostState>,
        stopped: Arc<Mutex<Option<StopOutcome>>>,
        thread: Option<thread::JoinHandle<()>>,
        directory: PathBuf,
    }

    impl Harness {
        fn start(name: &str, script: &str, timeouts: Timeouts) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "openchamber-managed-host-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory).expect("test directory");
            let launch = Launch {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".into(), script.into()],
                env: vec![
                    ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
                    ("NONCE".into(), NONCE.into()),
                    ("PORT".into(), PORT.to_string().into()),
                    ("WORK".into(), directory.clone().into()),
                ],
                port: PORT,
                nonce: NONCE.to_owned(),
                log: Some(directory.join("server.log")),
            };
            let (state, states) = watch::channel(HostState::Starting);
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
            let thread = thread::spawn(move || supervisor.run());
            Self {
                commands,
                states,
                stopped,
                thread: Some(thread),
                directory,
            }
        }

        /// Waits for a state, collecting every distinct one on the way.
        fn wait_for(&mut self, wanted: HostState, seen: &mut Vec<HostState>) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let current = *self.states.borrow_and_update();
                if seen.last() != Some(&current) {
                    seen.push(current);
                }
                if current == wanted {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "never reached {wanted:?}; saw {seen:?}"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn pid(&self, name: &str) -> ProcessIdentity {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(text) = std::fs::read_to_string(self.directory.join(name))
                    && let Ok(pid) = text.trim().parse()
                    && let Some(identity) = owned_process::identify(pid)
                {
                    return identity;
                }
                assert!(Instant::now() < deadline, "{name} never wrote its PID");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn quit(&mut self) {
            self.finish();
            assert_eq!(*self.states.borrow(), HostState::Stopped);
        }

        fn finish(&mut self) {
            let _ = self.commands.send(Message::Quit);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.finish();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn fast() -> Timeouts {
        Timeouts {
            ready: Duration::from_secs(10),
            stop: Duration::from_secs(5),
            kill_grace: Duration::from_millis(50),
            reap: Duration::from_secs(5),
        }
    }

    fn gone(identity: ProcessIdentity) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while owned_process::is_alive(identity) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn readiness_lines_carry_the_nonce() {
        assert_eq!(parse_ready("abc ready 4321\n", "abc"), Some(4321));
        assert_eq!(parse_ready("  abc ready 4321  ", "abc"), Some(4321));
        assert_eq!(parse_ready("abd ready 4321", "abc"), None);
        assert_eq!(parse_ready("abc ready port", "abc"), None);
        assert_eq!(
            parse_ready("Starting OpenChamber on port 4321", "abc"),
            None
        );
    }

    #[test]
    fn a_server_that_stops_on_end_of_input_stops_gracefully() {
        let mut host = Harness::start(
            "graceful",
            r#"echo $$ > "$WORK/server"; echo "noise"; echo "$NONCE ready $PORT"; cat > /dev/null; exit 0"#,
            fast(),
        );
        let mut seen = Vec::new();
        host.wait_for(HostState::Ready { port: PORT }, &mut seen);
        assert_eq!(seen.first(), Some(&HostState::Starting));
        let server = host.pid("server");
        host.quit();
        assert_eq!(
            *host.stopped.lock().expect("stop outcome lock"),
            Some(StopOutcome::Graceful)
        );
        assert!(gone(server), "the owned server exited");
        let log = std::fs::read_to_string(host.directory.join("server.log")).expect("server log");
        assert!(log.contains("noise"), "the server's output is logged");
        assert!(!log.contains(NONCE), "the readiness line is not logged");
    }

    #[test]
    fn a_crashed_server_recovers_and_then_quits() {
        // The first run crashes once it is ready; the next one stays.
        let script = r#"
            echo $$ > "$WORK/server"
            echo "$NONCE ready $PORT"
            if [ ! -e "$WORK/crashed" ]; then : > "$WORK/crashed"; exit 3; fi
            cat > /dev/null
        "#;
        let mut host = Harness::start("crash", script, fast());
        let mut seen = Vec::new();
        host.wait_for(HostState::Recovering { attempt: 1 }, &mut seen);
        host.wait_for(HostState::Ready { port: PORT }, &mut seen);
        assert!(
            seen.contains(&HostState::Recovering { attempt: 1 }),
            "the crash was reported: {seen:?}"
        );
        let replacement = host.pid("server");
        host.quit();
        assert!(gone(replacement));
    }

    #[test]
    fn a_server_that_keeps_crashing_fails() {
        let mut host = Harness::start("crash-loop", r#"echo "$NONCE ready $PORT"; exit 3"#, fast());
        let mut seen = Vec::new();
        host.wait_for(HostState::Failed(HostFailure::Crashed), &mut seen);
        host.quit();
    }

    #[test]
    fn a_server_that_exits_during_startup_fails_without_restart() {
        let mut host = Harness::start("startup-exit", "exit 1", fast());
        let mut seen = Vec::new();
        host.wait_for(HostState::Failed(HostFailure::StartupExited), &mut seen);
        host.quit();
    }

    #[test]
    fn a_server_that_ignores_quit_is_forced_with_its_detached_children() {
        // A detached child in its own session, like the managed OpenCode,
        // and a server that ignores both end of input and terminate.
        let script = r#"
            trap '' TERM HUP INT
            setsid sh -c 'trap "" TERM; echo $$ > "$WORK/detached"; while :; do sleep 1; done' < /dev/null &
            echo $$ > "$WORK/server"
            echo "$NONCE ready $PORT"
            exec 0< /dev/null
            while :; do sleep 1; done
        "#;
        let mut timeouts = fast();
        timeouts.stop = Duration::from_millis(500);
        let mut host = Harness::start("forced", script, timeouts);
        let mut seen = Vec::new();
        host.wait_for(HostState::Ready { port: PORT }, &mut seen);
        let server = host.pid("server");
        let detached = host.pid("detached");
        host.quit();
        assert_eq!(
            *host.stopped.lock().expect("stop outcome lock"),
            Some(StopOutcome::Forced)
        );
        assert!(gone(server), "the forced server exited");
        assert!(gone(detached), "its detached child was stopped too");
    }

    #[test]
    fn an_unrelated_process_survives_the_forced_stop() {
        let mut unrelated = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("an unrelated process starts");
        let outside = owned_process::identify(unrelated.id()).expect("it runs");
        let script = r#"trap '' TERM; echo "$NONCE ready $PORT"; exec 0< /dev/null; while :; do sleep 1; done"#;
        let mut timeouts = fast();
        timeouts.stop = Duration::from_millis(300);
        let mut host = Harness::start("unrelated", script, timeouts);
        let mut seen = Vec::new();
        host.wait_for(HostState::Ready { port: PORT }, &mut seen);
        host.quit();
        assert!(
            owned_process::is_alive(outside),
            "only owned processes are stopped"
        );
        let _ = unrelated.kill();
        let _ = unrelated.wait();
    }
}
