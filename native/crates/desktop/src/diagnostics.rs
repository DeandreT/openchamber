//! Local timing markers for startup, switching and rebuilds.
//!
//! Markers carry counts and durations, never message or title text. With
//! `OPENCHAMBER_NATIVE_DIAGNOSTICS=1` each marker is also printed to stdout
//! as one `OPENCHAMBER_NATIVE_MARK {json}` line for measurement tooling.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The moment the process entered `main`.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// A diagnostic milestone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Marker {
    /// The platform window exists.
    WindowCreated,
    /// The first frame with sidebar and transcript content was painted.
    ContentPainted,
    /// The composer is focused and accepts input.
    ComposerUsable,
    /// A selected session's transcript was revealed.
    SwitchCompleted,
    /// The sidebar projection was rebuilt.
    SidebarRebuilt,
    /// A synthetic response finished streaming.
    PlaybackCompleted,
    /// The first complete global session list was painted.
    SessionsShown,
    /// A visited session's first history read was painted.
    HistoryShown,
}

impl Marker {
    fn as_str(self) -> &'static str {
        match self {
            Self::WindowCreated => "window-created",
            Self::ContentPainted => "content-painted",
            Self::ComposerUsable => "composer-usable",
            Self::SwitchCompleted => "switch-completed",
            Self::SidebarRebuilt => "sidebar-rebuilt",
            Self::PlaybackCompleted => "playback-completed",
            Self::SessionsShown => "sessions-shown",
            Self::HistoryShown => "history-shown",
        }
    }
}

/// Records the process start; call first thing in `main`.
pub fn start() {
    PROCESS_START.get_or_init(Instant::now);
}

fn since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

fn emit_stdout() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("OPENCHAMBER_NATIVE_DIAGNOSTICS").is_ok_and(|value| value == "1")
    })
}

/// Records `marker` with an optional measured duration and count.
pub fn mark(marker: Marker, duration: Option<Duration>, count: Option<usize>) {
    let since_start_ms = since_start().as_secs_f64() * 1_000.0;
    let duration_ms = duration.map(|duration| duration.as_secs_f64() * 1_000.0);
    tracing::info!(
        marker = marker.as_str(),
        since_start_ms,
        duration_ms,
        count,
        "diagnostic marker"
    );
    if emit_stdout() {
        let mut line = format!(
            "OPENCHAMBER_NATIVE_MARK {{\"marker\":\"{}\",\"sinceStartMs\":{since_start_ms:.3}",
            marker.as_str()
        );
        if let Some(duration_ms) = duration_ms {
            line.push_str(&format!(",\"durationMs\":{duration_ms:.3}"));
        }
        if let Some(count) = count {
            line.push_str(&format!(",\"count\":{count}"));
        }
        line.push('}');
        println!("{line}");
    }
}
