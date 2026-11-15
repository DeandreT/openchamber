//! The facts under a finished turn, as `MessageBody.tsx` shows them: the
//! model, then the agent and the turn's duration. The effort is the model's
//! tooltip and the time the duration's.

/// A turn's duration: tenths of a second under a minute (`0.6s`), whole
/// seconds after (`1m 2s`), as `formatTurnDuration` writes it.
pub fn turn_duration(duration_ms: i64) -> String {
    if duration_ms < 60_000 {
        // `toFixed(1)` rounds half away from zero on these magnitudes.
        let tenths = (duration_ms as f64 / 100.0).round() as i64;
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    let total_seconds = (duration_ms as f64 / 1000.0).round() as i64;
    format!("{}m {}s", total_seconds / 60, total_seconds % 60)
}

/// The effort in the model's tooltip: `None` for the default efforts, else
/// the effort with its first character lowercased.
pub fn effort_fact(variant: &str) -> Option<String> {
    let lowered = variant.to_lowercase();
    if variant.is_empty() || lowered == "default" || lowered == "none" {
        return None;
    }
    let mut characters = variant.chars();
    let first = characters.next()?;
    Some(first.to_lowercase().chain(characters).collect())
}

/// The measured widths of a footer's facts, each with its own separator and
/// spacing; `None` for a fact the turn does not have.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FactWidths {
    /// The model fact: its icon, gap and full name.
    pub model: f32,
    pub agent: Option<f32>,
    pub duration: Option<f32>,
}

/// Which optional facts stay on the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShownFacts {
    pub agent: bool,
}

/// `useFactsFit`: drops the agent, the one fact with a priority, when the
/// model's name is not whole in `available`; the duration always stays, and a
/// model that still does not fit truncates.
pub fn fit_facts(available: f32, widths: FactWidths) -> ShownFacts {
    // The web allows one pixel of rounding.
    let agent = widths.agent.is_some_and(|agent| {
        widths.model + agent + widths.duration.unwrap_or(0.0) <= available + 1.0
    });
    ShownFacts { agent }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_the_web_writes_them() {
        assert_eq!(turn_duration(600), "0.6s");
        assert_eq!(turn_duration(2_249), "2.2s");
        assert_eq!(turn_duration(59_940), "59.9s");
        assert_eq!(turn_duration(62_400), "1m 2s");
        assert_eq!(turn_duration(119_600), "2m 0s");
    }

    #[test]
    fn default_efforts_are_left_out() {
        assert_eq!(effort_fact("default"), None);
        assert_eq!(effort_fact("None"), None);
        assert_eq!(effort_fact("High"), Some("high".to_owned()));
        assert_eq!(effort_fact("xhigh"), Some("xhigh".to_owned()));
    }

    /// The agent stays while the model, the agent and the duration fit, with
    /// the web's one pixel of rounding.
    #[test]
    fn the_agent_is_the_only_fact_that_drops() {
        let widths = FactWidths {
            model: 224.4,
            agent: Some(48.9),
            duration: Some(42.3),
        };
        let shown = |agent| ShownFacts { agent };
        assert_eq!(fit_facts(422.0, widths), shown(true));
        assert_eq!(
            fit_facts(315.0, widths),
            shown(true),
            "one pixel is allowed"
        );
        assert_eq!(fit_facts(314.0, widths), shown(false));
        assert_eq!(fit_facts(100.0, widths), shown(false));
        let without_agent = FactWidths {
            agent: None,
            ..widths
        };
        assert_eq!(fit_facts(422.0, without_agent), shown(false));
        let without_duration = FactWidths {
            duration: None,
            ..widths
        };
        assert_eq!(fit_facts(273.0, without_duration), shown(true));
        assert_eq!(fit_facts(272.0, without_duration), shown(false));
    }
}
