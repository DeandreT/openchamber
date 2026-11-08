//! IANA time zones from compiled zoneinfo (TZif) data, for the scheduled
//! task editor's next-run preview and its timezone list.
//!
//! Only parsing and offset lookup live here; reading `/usr/share/zoneinfo`
//! is the caller's. A zone answers the UTC offset in force at any instant:
//! from its transition table while one covers the instant, then from the
//! POSIX rule in the file's footer, which slim zoneinfo builds rely on for
//! every year after their last listed change.

/// Seconds since the Unix epoch.
pub type UnixSeconds = i64;

/// The legacy IANA names some ICU builds still expose, shown and stored
/// under their current names (`lib/timezones.ts`).
const LEGACY_ALIASES: &[(&str, &str)] = &[
    ("Europe/Kiev", "Europe/Kyiv"),
    ("Asia/Calcutta", "Asia/Kolkata"),
    ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
    ("Asia/Rangoon", "Asia/Yangon"),
    ("Asia/Katmandu", "Asia/Kathmandu"),
    ("America/Godthab", "America/Nuuk"),
    ("Atlantic/Faeroe", "Atlantic/Faroe"),
    ("Pacific/Enderbury", "Pacific/Kanton"),
];

/// The zones the editor offers when the system has no zone list.
pub const FALLBACK_ZONES: &[&str] = &[
    "UTC",
    "Europe/Kyiv",
    "Europe/London",
    "Europe/Berlin",
    "America/New_York",
    "America/Los_Angeles",
    "Asia/Tokyo",
];

/// `canonicalizeTimezone`: the current name of a renamed zone.
pub fn canonicalize(name: &str) -> &str {
    LEGACY_ALIASES
        .iter()
        .find(|(legacy, _)| *legacy == name)
        .map_or(name, |(_, current)| current)
}

/// The zone names of a `tzdata.zi` file (`Z <name> ...` lines), canonical,
/// with `UTC` first, deduplicated and otherwise sorted.
pub fn zone_names_from_tzdata(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|line| line.strip_prefix("Z "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|name| canonicalize(name).to_owned())
        .collect();
    names.sort();
    names.dedup();
    names.retain(|name| name != "UTC");
    names.insert(0, "UTC".to_owned());
    names
}

/// One rule-switch date of a POSIX TZ string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleDate {
    /// `Jn`: day 1 to 365, February 29 never counted.
    Julian(u16),
    /// `n`: day 0 to 365, February 29 counted in leap years.
    ZeroBased(u16),
    /// `Mm.w.d`: weekday `d` of week `w` (5 = last) of month `m`.
    MonthWeekDay { month: u8, week: u8, weekday: u8 },
}

/// A POSIX TZ rule: a standard offset and an optional daylight period.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PosixRule {
    /// Seconds east of UTC.
    standard: i32,
    daylight: Option<DaylightRule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DaylightRule {
    offset: i32,
    start: RuleDate,
    /// Local standard time of day the period starts, in seconds.
    start_time: i32,
    end: RuleDate,
    /// Local daylight time of day the period ends, in seconds.
    end_time: i32,
}

/// A parsed zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeZone {
    /// Transition instants, ascending, and the offset each one starts.
    transitions: Vec<(UnixSeconds, i32)>,
    /// The offset before the first transition.
    initial: i32,
    rule: Option<PosixRule>,
}

impl TimeZone {
    /// Coordinated Universal Time.
    pub fn utc() -> Self {
        Self {
            transitions: Vec::new(),
            initial: 0,
            rule: None,
        }
    }

    /// Parses TZif data of any version; `None` when it is not TZif.
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let header = Header::read(bytes)?;
        if header.version == 0 {
            return header.zone(bytes, 44, 4, None);
        }
        // Version 2 and later repeat the data with 64-bit times after the
        // version 1 block, then a newline-framed POSIX rule.
        let second = 44 + header.block_length(4);
        let header2 = Header::read(bytes.get(second..)?)?;
        let data = second + 44;
        let footer_start = data + header2.block_length(8);
        let footer = bytes
            .get(footer_start..)
            .and_then(|rest| rest.strip_prefix(b"\n"))
            .and_then(|rest| rest.split(|byte| *byte == b'\n').next())
            .and_then(|rule| std::str::from_utf8(rule).ok())
            .and_then(parse_posix_rule);
        header2.zone(bytes, data, 8, footer)
    }

    /// Seconds east of UTC at `instant`.
    pub fn offset_at(&self, instant: UnixSeconds) -> i32 {
        let after_table = self
            .transitions
            .last()
            .is_none_or(|(last, _)| instant >= *last);
        if after_table && let Some(rule) = &self.rule {
            return rule.offset_at(instant);
        }
        match self
            .transitions
            .partition_point(|(start, _)| *start <= instant)
        {
            0 => self.initial,
            index => self.transitions[index - 1].1,
        }
    }

    /// The instant a local wall-clock time names: the earlier one of a time
    /// that occurs twice, `None` for one a forward change skips.
    pub fn to_instant(&self, local: UnixSeconds) -> Option<UnixSeconds> {
        let mut candidates: Vec<UnixSeconds> = [
            self.offset_at(local - 86_400),
            self.offset_at(local),
            self.offset_at(local + 86_400),
        ]
        .iter()
        .map(|offset| local - i64::from(*offset))
        .filter(|instant| *instant + i64::from(self.offset_at(*instant)) == local)
        .collect();
        candidates.sort_unstable();
        candidates.first().copied()
    }
}

struct Header {
    version: u8,
    is_ut: usize,
    is_std: usize,
    leap: usize,
    time: usize,
    types: usize,
    chars: usize,
}

impl Header {
    fn read(bytes: &[u8]) -> Option<Self> {
        if bytes.get(..4)? != b"TZif" {
            return None;
        }
        let version = match *bytes.get(4)? {
            0 => 0,
            byte @ b'1'..=b'9' => byte - b'0',
            _ => return None,
        };
        let count = |index: usize| -> Option<usize> {
            let at = 20 + index * 4;
            let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
            usize::try_from(u32::from_be_bytes(raw)).ok()
        };
        Some(Self {
            version,
            is_ut: count(0)?,
            is_std: count(1)?,
            leap: count(2)?,
            time: count(3)?,
            types: count(4)?,
            chars: count(5)?,
        })
    }

    fn block_length(&self, time_size: usize) -> usize {
        self.time * time_size
            + self.time
            + self.types * 6
            + self.chars
            + self.leap * (time_size + 4)
            + self.is_std
            + self.is_ut
    }

    fn zone(
        &self,
        bytes: &[u8],
        data: usize,
        time_size: usize,
        rule: Option<PosixRule>,
    ) -> Option<TimeZone> {
        let times = bytes.get(data..data + self.time * time_size)?;
        let indices_at = data + self.time * time_size;
        let indices = bytes.get(indices_at..indices_at + self.time)?;
        let types_at = indices_at + self.time;
        let types = bytes.get(types_at..types_at + self.types * 6)?;
        if self.types == 0 {
            return None;
        }
        let offset_of = |index: usize| -> Option<i32> {
            let raw: [u8; 4] = types.get(index * 6..index * 6 + 4)?.try_into().ok()?;
            Some(i32::from_be_bytes(raw))
        };
        let mut transitions = Vec::with_capacity(self.time);
        for (position, index) in indices.iter().enumerate() {
            let at = position * time_size;
            let instant = if time_size == 8 {
                i64::from_be_bytes(times.get(at..at + 8)?.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(times.get(at..at + 4)?.try_into().ok()?))
            };
            transitions.push((instant, offset_of(usize::from(*index))?));
        }
        // RFC 8536: before the first transition the first standard type, which
        // is in practice type 0.
        Some(TimeZone {
            transitions,
            initial: offset_of(0)?,
            rule,
        })
    }
}

/// Parses a POSIX TZ string such as `EST5EDT,M3.2.0,M11.1.0`.
fn parse_posix_rule(text: &str) -> Option<PosixRule> {
    let mut rest = text;
    skip_name(&mut rest)?;
    let standard = -parse_offset(&mut rest)?;
    if rest.is_empty() {
        return Some(PosixRule {
            standard,
            daylight: None,
        });
    }
    skip_name(&mut rest)?;
    let offset = if rest.starts_with(',') || rest.is_empty() {
        standard + 3600
    } else {
        -parse_offset(&mut rest)?
    };
    let rest = rest.strip_prefix(',')?;
    let (start, end) = rest.split_once(',')?;
    let (start, start_time) = parse_rule_date(start)?;
    let (end, end_time) = parse_rule_date(end)?;
    Some(PosixRule {
        standard,
        daylight: Some(DaylightRule {
            offset,
            start,
            start_time,
            end,
            end_time,
        }),
    })
}

fn skip_name(rest: &mut &str) -> Option<()> {
    if let Some(quoted) = rest.strip_prefix('<') {
        let end = quoted.find('>')?;
        *rest = &quoted[end + 1..];
        return Some(());
    }
    let length = rest
        .find(|character: char| !character.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    if length < 3 {
        return None;
    }
    *rest = &rest[length..];
    Some(())
}

/// `[+-]hh[:mm[:ss]]`, as seconds west of UTC the way POSIX writes them.
fn parse_offset(rest: &mut &str) -> Option<i32> {
    let (sign, body) = match rest.as_bytes().first()? {
        b'-' => (-1, &rest[1..]),
        b'+' => (1, &rest[1..]),
        _ => (1, *rest),
    };
    let length = body
        .find(|character: char| !(character.is_ascii_digit() || character == ':'))
        .unwrap_or(body.len());
    let seconds = parse_clock(&body[..length])?;
    *rest = &body[length..];
    Some(sign * seconds)
}

fn parse_clock(text: &str) -> Option<i32> {
    let mut parts = text.split(':');
    let hours: i32 = parts.next()?.parse().ok()?;
    let minutes: i32 = parts.next().map_or(Some(0), |part| part.parse().ok())?;
    let seconds: i32 = parts.next().map_or(Some(0), |part| part.parse().ok())?;
    Some(hours * 3600 + minutes * 60 + seconds)
}

fn parse_rule_date(text: &str) -> Option<(RuleDate, i32)> {
    let (date, time) = match text.split_once('/') {
        Some((date, time)) => {
            let (sign, clock) = match time.strip_prefix('-') {
                Some(clock) => (-1, clock),
                None => (1, time.strip_prefix('+').unwrap_or(time)),
            };
            (date, sign * parse_clock(clock)?)
        }
        None => (text, 7200),
    };
    let date = if let Some(spec) = date.strip_prefix('M') {
        let mut parts = spec.split('.');
        let month: u8 = parts.next()?.parse().ok()?;
        let week: u8 = parts.next()?.parse().ok()?;
        let weekday: u8 = parts.next()?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
            return None;
        }
        RuleDate::MonthWeekDay {
            month,
            week,
            weekday,
        }
    } else if let Some(day) = date.strip_prefix('J') {
        RuleDate::Julian(day.parse().ok().filter(|day| (1..=365).contains(day))?)
    } else {
        RuleDate::ZeroBased(date.parse().ok().filter(|day| *day <= 365)?)
    };
    Some((date, time))
}

impl PosixRule {
    fn offset_at(&self, instant: UnixSeconds) -> i32 {
        let Some(daylight) = &self.daylight else {
            return self.standard;
        };
        // The year of local standard time decides which year's switches apply.
        let year = civil_from_days((instant + i64::from(self.standard)).div_euclid(86_400)).0;
        let start = daylight.start.local_midnight(year) + i64::from(daylight.start_time)
            - i64::from(self.standard);
        let end = daylight.end.local_midnight(year) + i64::from(daylight.end_time)
            - i64::from(daylight.offset);
        let within = if start < end {
            instant >= start && instant < end
        } else {
            // Southern hemisphere: daylight time spans the new year.
            instant >= start || instant < end
        };
        if within {
            daylight.offset
        } else {
            self.standard
        }
    }
}

impl RuleDate {
    /// Seconds from the epoch to local midnight of the rule's day in `year`.
    fn local_midnight(self, year: i64) -> i64 {
        let days = match self {
            Self::Julian(day) => {
                let mut day = i64::from(day) - 1;
                if is_leap(year) && day >= 59 {
                    day += 1;
                }
                days_from_civil(year, 1, 1) + day
            }
            Self::ZeroBased(day) => days_from_civil(year, 1, 1) + i64::from(day),
            Self::MonthWeekDay {
                month,
                week,
                weekday,
            } => {
                let first = days_from_civil(year, u32::from(month), 1);
                let first_weekday = weekday_of(first);
                let mut day = first + (i64::from(weekday) - first_weekday).rem_euclid(7);
                day += 7 * (i64::from(week) - 1);
                let length = i64::from(days_in_month(year, u32::from(month)));
                while day >= first + length {
                    day -= 7;
                }
                day
            }
        };
        days * 86_400
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day since 1970-01-01: year, month 1 to 12, day 1 to 31.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted + 2) / 5 + 1) as u32;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// 0 for Sunday through 6 for Saturday.
pub fn weekday_of(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

pub fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version 2 TZif file with no transitions and `rule` as its footer, the
    /// shape of slim zoneinfo for a zone without listed history.
    fn slim(rule: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        let header = |bytes: &mut Vec<u8>, version: u8| {
            bytes.extend_from_slice(b"TZif");
            bytes.push(version);
            bytes.extend_from_slice(&[0; 15]);
            for count in [0_u32, 0, 0, 0, 1, 4] {
                bytes.extend_from_slice(&count.to_be_bytes());
            }
        };
        let block = |bytes: &mut Vec<u8>| {
            bytes.extend_from_slice(&0_i32.to_be_bytes());
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(b"UTC\0");
        };
        header(&mut bytes, b'2');
        block(&mut bytes);
        header(&mut bytes, b'2');
        block(&mut bytes);
        bytes.push(b'\n');
        bytes.extend_from_slice(rule.as_bytes());
        bytes.push(b'\n');
        bytes
    }

    fn instant(year: i64, month: u32, day: u32, hour: i64, minute: i64) -> i64 {
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in [-800_000, -1, 0, 59, 11_016, 20_000, 2_000_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday_of(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn the_footer_rule_decides_daylight_time() {
        let zone = TimeZone::from_tzif(&slim("EST5EDT,M3.2.0,M11.1.0")).expect("TZif parses");
        assert_eq!(zone.offset_at(instant(2030, 1, 15, 12, 0)), -5 * 3600);
        assert_eq!(zone.offset_at(instant(2030, 7, 1, 12, 0)), -4 * 3600);
        // 2030-03-10 02:00 EST is 07:00 UTC; the clock jumps to 03:00 EDT.
        assert_eq!(zone.offset_at(instant(2030, 3, 10, 6, 59)), -5 * 3600);
        assert_eq!(zone.offset_at(instant(2030, 3, 10, 7, 0)), -4 * 3600);
        // A skipped wall time names no instant; a repeated one the earlier.
        assert_eq!(zone.to_instant(instant(2030, 3, 10, 2, 30)), None);
        assert_eq!(
            zone.to_instant(instant(2030, 11, 3, 1, 30)),
            Some(instant(2030, 11, 3, 5, 30))
        );
    }

    #[test]
    fn southern_rules_span_the_new_year() {
        let zone = TimeZone::from_tzif(&slim("AEST-10AEDT,M10.1.0,M4.1.0/3")).expect("parses");
        assert_eq!(zone.offset_at(instant(2031, 1, 1, 0, 0)), 11 * 3600);
        assert_eq!(zone.offset_at(instant(2031, 6, 1, 0, 0)), 10 * 3600);
        let fixed = TimeZone::from_tzif(&slim("<+0530>-5:30")).expect("parses");
        assert_eq!(fixed.offset_at(0), 5 * 3600 + 1800);
    }

    #[test]
    fn transitions_decide_before_the_footer() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"TZif");
        bytes.push(0);
        bytes.extend_from_slice(&[0; 15]);
        for count in [0_u32, 0, 0, 1, 2, 4] {
            bytes.extend_from_slice(&count.to_be_bytes());
        }
        bytes.extend_from_slice(&1000_i32.to_be_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&0_i32.to_be_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&3600_i32.to_be_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(b"AB\0\0");
        let zone = TimeZone::from_tzif(&bytes).expect("version 1 parses");
        assert_eq!(zone.offset_at(999), 0);
        assert_eq!(zone.offset_at(1000), 3600);
        assert!(TimeZone::from_tzif(b"nope").is_none());
    }

    #[test]
    fn zone_lists_are_canonical_with_utc_first() {
        let names = zone_names_from_tzdata(
            "# version\nR US 1967 ...\nZ Europe/Kiev 2 - EET\nZ America/New_York -5 - E%sT\nL Europe/Kyiv Europe/Kiev2\nZ Etc/UTC 0 - UTC\n",
        );
        assert_eq!(names, ["UTC", "America/New_York", "Etc/UTC", "Europe/Kyiv"]);
        assert_eq!(canonicalize("Asia/Calcutta"), "Asia/Kolkata");
        assert_eq!(canonicalize("Asia/Tokyo"), "Asia/Tokyo");
    }
}
