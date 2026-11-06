//! One once-a-second clock for the transcript's live indicators: running
//! durations and busy dots (`useDurationTicker`'s shared clock).
//!
//! A frame that draws a live indicator marks the clock; each tick redraws
//! only if the last frame drew one, and the clock stops once a tick finds no
//! mark. Nothing ticks while no live indicator is on screen.

use std::cell::Cell;
use std::time::Duration;

use gpui_kit::Context;

use crate::workspace::Workspace;

const TICK: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct TranscriptClock {
    drawn: Cell<bool>,
    ticking: Cell<bool>,
}

impl TranscriptClock {
    /// Records that this frame draws a live indicator.
    pub(crate) fn mark(&self) {
        self.drawn.set(true);
    }

    /// Takes the mark for one tick.
    fn take(&self) -> bool {
        self.drawn.replace(false)
    }
}

/// Wall-clock milliseconds, the unit of message and part times.
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

impl Workspace {
    /// Marks the frame and starts the clock when it is not running.
    pub(crate) fn tick_live_indicators(&self, cx: &Context<Self>) {
        self.transcript_clock.mark();
        if self.transcript_clock.ticking.replace(true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep = this
                    .update(cx, |this, cx| {
                        let keep = this.transcript_clock.take();
                        if keep {
                            cx.notify();
                        } else {
                            this.transcript_clock.ticking.set(false);
                        }
                        keep
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_lasts_one_tick() {
        let clock = TranscriptClock::default();
        assert!(!clock.take());
        clock.mark();
        assert!(clock.take());
        assert!(!clock.take(), "an undrawn indicator stops the clock");
    }
}
