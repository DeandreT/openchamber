//! The facts under a finished turn, as `MessageBody.tsx` shows them: the
//! model, then the effort, the agent, the turn's duration and the time.

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

/// The effort fact: `None` for the default efforts, else the effort with its
/// first character lowercased.
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
    pub effort: Option<f32>,
    pub agent: Option<f32>,
    pub duration: Option<f32>,
    pub timestamp: Option<f32>,
}

/// Which optional facts stay on the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShownFacts {
    pub effort: bool,
    pub agent: bool,
    pub timestamp: bool,
}

/// `useFactsFit`: drops the time, then the agent, then the effort until the
/// model's name is whole in `available`; the duration always stays, and a
/// model that still does not fit truncates.
pub fn fit_facts(available: f32, widths: FactWidths) -> ShownFacts {
    let mut shown = ShownFacts {
        effort: widths.effort.is_some(),
        agent: widths.agent.is_some(),
        timestamp: widths.timestamp.is_some(),
    };
    let model_fits = |shown: &ShownFacts| {
        let optional = [
            (shown.effort, widths.effort),
            (shown.agent, widths.agent),
            (shown.timestamp, widths.timestamp),
        ]
        .into_iter()
        .filter_map(|(kept, width)| kept.then_some(width).flatten())
        .sum::<f32>();
        // The web allows one pixel of rounding.
        widths.model + optional + widths.duration.unwrap_or(0.0) <= available + 1.0
    };
    for drop in [
        |shown: &mut ShownFacts| shown.timestamp = false,
        |shown: &mut ShownFacts| shown.agent = false,
        |shown: &mut ShownFacts| shown.effort = false,
    ] {
        if model_fits(&shown) {
            return shown;
        }
        drop(&mut shown);
    }
    shown
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

    /// The reference's 422 px row: model, agent and duration fit; the time
    /// does not.
    #[test]
    fn facts_drop_in_priority_order() {
        let widths = FactWidths {
            model: 224.4,
            effort: None,
            agent: Some(48.9),
            duration: Some(42.3),
            timestamp: Some(131.0),
        };
        let shown = |effort, agent, timestamp| ShownFacts {
            effort,
            agent,
            timestamp,
        };
        assert_eq!(fit_facts(422.0, widths), shown(false, true, false));
        assert_eq!(fit_facts(500.0, widths), shown(false, true, true));
        assert_eq!(fit_facts(300.0, widths), shown(false, false, false));
        let with_effort = FactWidths {
            effort: Some(40.0),
            ..widths
        };
        assert_eq!(fit_facts(310.0, with_effort), shown(true, false, false));
        assert_eq!(fit_facts(250.0, with_effort), shown(false, false, false));
    }
}
