//! The managed host's states and the transitions between them.
//!
//! Pure: the supervisor feeds it what happened to the owned server and acts
//! on the answer. Two rules matter most. Nothing starts a server once quit
//! was requested, and restarts after a crash are bounded: crashes within
//! [`CRASH_WINDOW`] of the previous one count as one in a row, and after
//! [`MAX_RESTARTS`] restarts in a row the host fails and waits for the user,
//! the way the desktop's renderer recovery stops reloading a window that
//! keeps crashing (`packages/electron/renderer-recovery.mjs`). A failed host
//! stays failed for the rest of the launch; Retry relaunches the app, as the
//! desktop's recovery screen does.

use std::time::{Duration, Instant};

/// Crashes this close to the previous one count as one in a row.
pub(crate) const CRASH_WINDOW: Duration = Duration::from_secs(120);
/// Restarts in a row before the host gives up.
pub(crate) const MAX_RESTARTS: u32 = 3;
/// The pause before a crashed server starts again.
pub(crate) const RESTART_DELAY: Duration = Duration::from_secs(1);

/// What the user sees of the managed host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostState {
    /// The first server of this launch is starting.
    Starting,
    /// The server listens on the loopback port.
    Ready { port: u16 },
    /// The server stopped unexpectedly and starts again.
    Recovering { attempt: u32 },
    /// No server runs; the user can retry.
    Failed(HostFailure),
    /// Quit was requested; the owned processes are stopping.
    Quitting,
    /// Every owned process has exited.
    Stopped,
}

/// Why the managed host failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostFailure {
    /// The bundled runtime, server or OpenCode is missing.
    BundleMissing,
    /// The runtime could not be started, or its identity could not be read.
    SpawnFailed,
    /// The first server exited before it was ready.
    StartupExited,
    /// A server did not become ready in time.
    StartupTimedOut,
    /// The server kept crashing.
    Crashed,
}

/// What happened to the owned server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Ready(u16),
    Exited,
    StartupTimedOut,
    SpawnFailed,
    QuitRequested,
    /// Every owned process exited after quit.
    Stopped,
}

/// What the supervisor does next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Next {
    /// Stop the running server and its descendants.
    pub(crate) stop_child: bool,
    /// Start a server after this delay.
    pub(crate) spawn_after: Option<Duration>,
}

/// The state machine.
#[derive(Debug)]
pub(crate) struct Lifecycle {
    state: HostState,
    crashes_in_a_row: u32,
    last_crash: Option<Instant>,
}

impl Lifecycle {
    /// A host that starts its first server now.
    pub(crate) fn starting() -> (Self, Next) {
        let lifecycle = Self {
            state: HostState::Starting,
            crashes_in_a_row: 0,
            last_crash: None,
        };
        (lifecycle, spawn_now())
    }

    pub(crate) fn state(&self) -> HostState {
        self.state
    }

    /// Whether a server should be running or starting.
    pub(crate) fn awaits_ready(&self) -> bool {
        matches!(
            self.state,
            HostState::Starting | HostState::Recovering { .. }
        )
    }

    pub(crate) fn apply(&mut self, event: Event, now: Instant) -> Next {
        use HostState::*;
        match (self.state, event) {
            (Stopped, _) => Next::default(),
            (Quitting, Event::Stopped) => {
                self.state = Stopped;
                Next::default()
            }
            // Quit owns the rest of the host's life: no restart, no retry.
            (Quitting, _) => Next::default(),
            (_, Event::QuitRequested) => {
                self.state = Quitting;
                Next {
                    stop_child: true,
                    spawn_after: None,
                }
            }
            (Starting | Recovering { .. }, Event::Ready(port)) => {
                self.state = Ready { port };
                Next::default()
            }
            (Starting, Event::Exited) => {
                self.state = Failed(HostFailure::StartupExited);
                Next::default()
            }
            (Starting, Event::StartupTimedOut) => {
                self.state = Failed(HostFailure::StartupTimedOut);
                Next {
                    stop_child: true,
                    spawn_after: None,
                }
            }
            (Starting | Recovering { .. }, Event::SpawnFailed) => {
                self.state = Failed(HostFailure::SpawnFailed);
                Next::default()
            }
            (Ready { .. } | Recovering { .. }, Event::Exited) => self.crash(now, false),
            (Recovering { .. }, Event::StartupTimedOut) => self.crash(now, true),
            _ => Next::default(),
        }
    }

    fn crash(&mut self, now: Instant, stop_child: bool) -> Next {
        let in_a_row = self
            .last_crash
            .is_some_and(|last| now.saturating_duration_since(last) <= CRASH_WINDOW);
        self.crashes_in_a_row = if in_a_row {
            self.crashes_in_a_row + 1
        } else {
            1
        };
        self.last_crash = Some(now);
        if self.crashes_in_a_row > MAX_RESTARTS {
            self.state = HostState::Failed(HostFailure::Crashed);
            return Next {
                stop_child,
                spawn_after: None,
            };
        }
        self.state = HostState::Recovering {
            attempt: self.crashes_in_a_row,
        };
        Next {
            stop_child,
            spawn_after: Some(RESTART_DELAY),
        }
    }
}

fn spawn_now() -> Next {
    Next {
        stop_child: false,
        spawn_after: Some(Duration::ZERO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 41_234;

    fn ready(now: Instant) -> (Lifecycle, Instant) {
        let (mut lifecycle, next) = Lifecycle::starting();
        assert_eq!(next.spawn_after, Some(Duration::ZERO));
        lifecycle.apply(Event::Ready(PORT), now);
        assert_eq!(lifecycle.state(), HostState::Ready { port: PORT });
        (lifecycle, now)
    }

    #[test]
    fn startup_failures_wait_for_the_user() {
        let now = Instant::now();
        let (mut exited, _) = Lifecycle::starting();
        assert_eq!(exited.apply(Event::Exited, now), Next::default());
        assert_eq!(
            exited.state(),
            HostState::Failed(HostFailure::StartupExited)
        );

        let (mut late, _) = Lifecycle::starting();
        let next = late.apply(Event::StartupTimedOut, now);
        assert!(next.stop_child, "a server that never got ready is stopped");
        assert_eq!(next.spawn_after, None);
        assert_eq!(
            late.state(),
            HostState::Failed(HostFailure::StartupTimedOut)
        );

        for event in [Event::Exited, Event::Ready(PORT)] {
            late.apply(event, now);
        }
        assert_eq!(
            late.state(),
            HostState::Failed(HostFailure::StartupTimedOut),
            "a failed host stays failed for the launch"
        );
    }

    #[test]
    fn crashes_restart_until_the_budget_runs_out() {
        let (mut lifecycle, start) = ready(Instant::now());
        for attempt in 1..=MAX_RESTARTS {
            let at = start + Duration::from_secs(u64::from(attempt));
            let next = lifecycle.apply(Event::Exited, at);
            assert_eq!(lifecycle.state(), HostState::Recovering { attempt });
            assert_eq!(next.spawn_after, Some(RESTART_DELAY));
            lifecycle.apply(Event::Ready(PORT), at);
        }
        let next = lifecycle.apply(Event::Exited, start + Duration::from_secs(10));
        assert_eq!(lifecycle.state(), HostState::Failed(HostFailure::Crashed));
        assert_eq!(next.spawn_after, None);
    }

    #[test]
    fn a_quiet_spell_starts_the_count_again() {
        let (mut lifecycle, start) = ready(Instant::now());
        for attempt in 1..=MAX_RESTARTS {
            lifecycle.apply(Event::Exited, start);
            lifecycle.apply(Event::Ready(PORT), start);
            assert_eq!(attempt, lifecycle.crashes_in_a_row);
        }
        lifecycle.apply(Event::Exited, start + CRASH_WINDOW + Duration::from_secs(1));
        assert_eq!(lifecycle.state(), HostState::Recovering { attempt: 1 });
    }

    #[test]
    fn a_restart_that_never_gets_ready_counts_as_a_crash() {
        let (mut lifecycle, start) = ready(Instant::now());
        lifecycle.apply(Event::Exited, start);
        let next = lifecycle.apply(Event::StartupTimedOut, start);
        assert!(next.stop_child);
        assert_eq!(lifecycle.state(), HostState::Recovering { attempt: 2 });
    }

    #[test]
    fn quit_stops_the_server_and_nothing_restarts_it() {
        let (mut lifecycle, now) = ready(Instant::now());
        let next = lifecycle.apply(Event::QuitRequested, now);
        assert!(next.stop_child);
        assert_eq!(lifecycle.state(), HostState::Quitting);
        for event in [
            Event::Exited,
            Event::Ready(PORT),
            Event::StartupTimedOut,
            Event::QuitRequested,
        ] {
            assert_eq!(lifecycle.apply(event, now), Next::default());
            assert_eq!(lifecycle.state(), HostState::Quitting);
        }
        lifecycle.apply(Event::Stopped, now);
        assert_eq!(lifecycle.state(), HostState::Stopped);
        assert_eq!(lifecycle.apply(Event::Exited, now), Next::default());
        assert_eq!(lifecycle.state(), HostState::Stopped);
    }

    #[test]
    fn quit_while_recovering_cancels_the_restart() {
        let (mut lifecycle, now) = ready(Instant::now());
        lifecycle.apply(Event::Exited, now);
        let next = lifecycle.apply(Event::QuitRequested, now);
        assert_eq!(next.spawn_after, None);
        assert_eq!(lifecycle.state(), HostState::Quitting);
    }
}
