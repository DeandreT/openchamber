//! The cron preview against `cron-parser`, the library the web editor uses.
//! The fixture holds its next four runs for each expression, zone and start;
//! zones come from the system's compiled zoneinfo, and a zone the system
//! lacks is skipped, UTC never.

use openchamber_native_core::schedule::CronExpression;
use openchamber_native_core::time_zone::TimeZone;
use serde_json::Value;

fn zone(name: &str) -> Option<TimeZone> {
    if name == "UTC" {
        return Some(TimeZone::utc());
    }
    let directory = std::env::var("TZDIR").unwrap_or_else(|_| "/usr/share/zoneinfo".to_owned());
    TimeZone::from_tzif(&std::fs::read(format!("{directory}/{name}")).ok()?)
}

#[test]
fn next_runs_match_cron_parser() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/cron-next-runs.json")).expect("fixture");
    let mut compared = 0;
    for case in fixture["cases"].as_array().expect("cases") {
        let expression = case["e"].as_str().expect("expression");
        let Some(zone) = zone(case["tz"].as_str().expect("zone")) else {
            continue;
        };
        let start = case["s"].as_i64().expect("start");
        let expected: Vec<i64> = case["runs"]
            .as_array()
            .expect("runs")
            .iter()
            .map(|run| run.as_i64().expect("a run"))
            .collect();
        let runs = CronExpression::parse(expression)
            .expect("cron-parser accepted it")
            .next_runs(&zone, start, 4);
        assert_eq!(
            runs, expected,
            "{expression} in {} from {start}",
            case["tz"]
        );
        compared += 1;
    }
    assert!(compared >= 42, "at least the UTC cases ran");
}
