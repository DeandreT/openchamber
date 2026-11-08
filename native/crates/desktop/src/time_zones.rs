//! The system's IANA time zones for the scheduled task editor: the list it
//! offers, the local zone a new task starts in, and each zone's rules for
//! the cron preview. Read from the compiled zoneinfo (`$TZDIR`, else
//! `/usr/share/zoneinfo`); `core::time_zone` parses it.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use openchamber_native_core::time_zone::{self, FALLBACK_ZONES, TimeZone};

fn zoneinfo() -> PathBuf {
    std::env::var_os("TZDIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"))
}

/// Every zone the system knows, `UTC` first; the reference's short list
/// when the system lists none.
pub(crate) fn zone_names() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let names = std::fs::read_to_string(zoneinfo().join("tzdata.zi"))
            .map(|text| time_zone::zone_names_from_tzdata(&text))
            .unwrap_or_default();
        if names.len() > 1 {
            names
        } else {
            FALLBACK_ZONES
                .iter()
                .map(|zone| (*zone).to_owned())
                .collect()
        }
    })
}

/// A zone name that stays inside the zoneinfo folder.
fn zone_path(name: &str) -> Option<PathBuf> {
    let relative = Path::new(name);
    relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| zoneinfo().join(relative))
}

/// A zone's rules, read once; `None` for a name the system does not have.
pub(crate) fn zone(name: &str) -> Option<TimeZone> {
    if name == "UTC" {
        return Some(TimeZone::utc());
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Option<TimeZone>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().ok()?;
    cache
        .entry(name.to_owned())
        .or_insert_with(|| {
            let bytes = std::fs::read(zone_path(name)?).ok()?;
            TimeZone::from_tzif(&bytes)
        })
        .clone()
}

/// The local zone: `$TZ` when it names one, else where `/etc/localtime`
/// points, else UTC. Canonical, as the editor stores it.
pub(crate) fn local_zone_name() -> String {
    let from_env = std::env::var("TZ")
        .ok()
        .map(|value| value.trim_start_matches(':').to_owned())
        .filter(|value| !value.is_empty() && zone(value).is_some());
    let from_link = || {
        let target = std::fs::read_link("/etc/localtime").ok()?;
        let text = target.to_str()?;
        let (_, name) = text.split_once("zoneinfo/")?;
        Some(name.trim_start_matches("posix/").to_owned())
    };
    let name = from_env
        .or_else(from_link)
        .unwrap_or_else(|| "UTC".to_owned());
    time_zone::canonicalize(&name).to_owned()
}

/// Today's date in the local zone, `YYYY-MM-DD`.
pub(crate) fn local_today(now_seconds: i64) -> String {
    let offset = zone(&local_zone_name()).map_or(0, |zone| zone.offset_at(now_seconds));
    let (year, month, day) =
        time_zone::civil_from_days((now_seconds + i64::from(offset)).div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_names_cannot_leave_the_zoneinfo_folder() {
        assert!(zone_path("../../etc/passwd").is_none());
        assert!(zone_path("/etc/passwd").is_none());
        assert!(zone_path("Europe/Berlin").is_some());
        assert!(zone("UTC").is_some());
        assert!(!zone_names().is_empty());
        assert_eq!(zone_names()[0], "UTC");
    }
}
