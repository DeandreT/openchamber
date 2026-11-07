//! Identity and signals for processes the managed host started.
//!
//! A process is identified by its PID together with its start time in clock
//! ticks since boot (`/proc/<pid>/stat`, field 22). A PID alone can name an
//! unrelated process once the original exits and the number is reused; the
//! pair cannot. Ownership is never inferred from an executable name: a
//! process is owned when the host spawned it, or when it descended from such
//! a process while that process was still running.
//!
//! Signals go through the system `kill` program, checked the way the HEIC
//! worker checks `prlimit`: the crate forbids `unsafe` and carries no libc
//! binding. Every signal re-reads the target's identity right before it is
//! sent and refuses a process whose start time no longer matches.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// The kill programs a signal may use, in order.
const KILL_PROGRAMS: [&str; 2] = ["/usr/bin/kill", "/bin/kill"];

/// A running process: its PID and the moment it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    /// Clock ticks since boot at which the process started.
    pub(crate) start_ticks: u64,
}

/// One entry of a descendant snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnedProcess {
    pub(crate) identity: ProcessIdentity,
    /// The process group. A detached child such as the managed OpenCode leads
    /// its own group (`pgid == pid`), and its tools run in that group.
    pub(crate) group: u32,
}

impl OwnedProcess {
    pub(crate) fn leads_group(&self) -> bool {
        self.group == self.identity.pid
    }
}

/// The signals the host sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    Terminate,
    Kill,
}

impl Signal {
    fn name(self) -> &'static str {
        match self {
            Self::Terminate => "TERM",
            Self::Kill => "KILL",
        }
    }
}

/// Whether a signal reaches one process or the group it leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Process,
    Group,
}

/// Why a signal was not sent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignalError {
    /// The PID is gone or now names another process.
    NotOwned,
    /// No trusted kill program, or it could not run.
    Unavailable,
    /// The kill program refused, for example because the group is empty.
    Refused,
}

/// The fields of `/proc/<pid>/stat` the host uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    parent: u32,
    group: u32,
    start_ticks: u64,
}

/// Parses a stat line. The command name sits in parentheses and may itself
/// contain spaces and parentheses, so fields are counted from the last `)`.
fn parse_stat(line: &str) -> Option<Stat> {
    let after_name = &line[line.rfind(')')? + 1..];
    // Field 3 (state) is the first after the name; 4 is the parent, 5 the
    // group and 22 the start time.
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    Some(Stat {
        parent: fields.get(1)?.parse().ok()?,
        group: fields.get(2)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

fn read_stat(proc_root: &Path, pid: u32) -> Option<Stat> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    parse_stat(&text)
}

/// The identity of a running process, or `None` when it is gone.
pub(crate) fn identify(pid: u32) -> Option<ProcessIdentity> {
    read_stat(Path::new("/proc"), pid).map(|stat| ProcessIdentity {
        pid,
        start_ticks: stat.start_ticks,
    })
}

/// Whether `identity` still names a running process.
pub(crate) fn is_alive(identity: ProcessIdentity) -> bool {
    identify(identity.pid) == Some(identity)
}

/// Every running descendant of `root`, found through parent links.
///
/// Empty when `root` itself is gone or was replaced.
pub(crate) fn descendants(root: ProcessIdentity) -> Vec<OwnedProcess> {
    descendants_in(Path::new("/proc"), root)
}

fn descendants_in(proc_root: &Path, root: ProcessIdentity) -> Vec<OwnedProcess> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut stats: HashMap<u32, Stat> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if let Some(stat) = read_stat(proc_root, pid) {
            stats.insert(pid, stat);
        }
    }
    if stats.get(&root.pid).map(|stat| stat.start_ticks) != Some(root.start_ticks) {
        return Vec::new();
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, stat) in &stats {
        children.entry(stat.parent).or_default().push(*pid);
    }
    let mut found = Vec::new();
    let mut pending = vec![root.pid];
    while let Some(parent) = pending.pop() {
        for child in children.get(&parent).into_iter().flatten() {
            let stat = stats[child];
            found.push(OwnedProcess {
                identity: ProcessIdentity {
                    pid: *child,
                    start_ticks: stat.start_ticks,
                },
                group: stat.group,
            });
            pending.push(*child);
        }
    }
    found
}

/// Sends `signal` to an owned process, or to the group it leads.
pub(crate) fn send(
    identity: ProcessIdentity,
    target: Target,
    signal: Signal,
) -> Result<(), SignalError> {
    let program = trusted_kill_program().ok_or(SignalError::Unavailable)?;
    if !is_alive(identity) {
        return Err(SignalError::NotOwned);
    }
    let destination = match target {
        Target::Process => identity.pid.to_string(),
        Target::Group => format!("-{}", identity.pid),
    };
    let status = Command::new(program)
        .args(["-s", signal.name(), "--", &destination])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| SignalError::Unavailable)?;
    if status.success() {
        Ok(())
    } else {
        Err(SignalError::Refused)
    }
}

/// The first kill program owned by root that nobody else can replace.
fn trusted_kill_program() -> Option<&'static str> {
    KILL_PROGRAMS
        .into_iter()
        .find(|path| is_trusted_program(Path::new(path)).unwrap_or(false))
}

fn is_trusted_program(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    Ok(metadata.is_file()
        && metadata.uid() == 0
        && metadata.mode() & 0o6022 == 0
        && metadata.mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Child;
    use std::time::{Duration, Instant};

    fn spawn_sleep() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts")
    }

    fn wait_gone(child: &mut Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().expect("child state").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn stat_fields_are_counted_after_the_last_parenthesis() {
        let line = "42 (a (b) c) S 7 42 42 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 98765 0 0";
        assert_eq!(
            parse_stat(line),
            Some(Stat {
                parent: 7,
                group: 42,
                start_ticks: 98765,
            })
        );
        assert_eq!(parse_stat("42 (truncated"), None);
        assert_eq!(parse_stat("42 (x) S 7"), None);
    }

    #[test]
    fn a_reused_pid_is_not_signalled() {
        let mut child = spawn_sleep();
        let identity = identify(child.id()).expect("the child is running");
        let stale = ProcessIdentity {
            start_ticks: identity.start_ticks.wrapping_add(1),
            ..identity
        };
        assert!(!is_alive(stale), "another start time names another process");
        assert_eq!(
            send(stale, Target::Process, Signal::Kill),
            Err(SignalError::NotOwned)
        );
        assert!(
            child.try_wait().expect("child state").is_none(),
            "the refused signal left the process running"
        );
        send(identity, Target::Process, Signal::Kill).expect("the owned process is signalled");
        assert!(wait_gone(&mut child), "the owned process stopped");
    }

    #[test]
    fn descendants_follow_parent_links_from_a_live_root() {
        let mut shell = Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30 & wait"])
            .stdin(Stdio::null())
            .spawn()
            .expect("shell starts");
        let root = identify(shell.id()).expect("the shell is running");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = descendants(root);
        while found.len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            found = descendants(root);
        }
        assert_eq!(found.len(), 2, "both sleeps descend from the shell");
        assert!(found.iter().all(|process| is_alive(process.identity)));
        let replaced = ProcessIdentity {
            start_ticks: root.start_ticks.wrapping_add(1),
            ..root
        };
        assert!(
            descendants(replaced).is_empty(),
            "a replaced root owns nothing"
        );
        for process in &found {
            let _ = send(process.identity, Target::Process, Signal::Kill);
        }
        let _ = shell.kill();
        let _ = shell.wait();
    }
}
