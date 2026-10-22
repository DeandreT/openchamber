//! Compact ages of sidebar rows, as `formatSessionCompactDateLabel` in
//! `packages/ui/src/components/session/sidebar/utils.tsx` buckets them.

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// The unit of a compact age.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgeUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

/// An age such as `12m` or `3w`: a count of whole units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompactAge {
    pub unit: AgeUnit,
    pub count: i64,
}

/// The compact age of `at_ms` seen at `now_ms`. A time in the future reads as
/// one minute, never a negative age.
pub fn compact_age(now_ms: i64, at_ms: i64) -> CompactAge {
    let elapsed = now_ms.saturating_sub(at_ms).max(0);
    let (unit, count) = if elapsed < HOUR {
        (AgeUnit::Minutes, (elapsed / MINUTE).max(1))
    } else if elapsed < DAY {
        (AgeUnit::Hours, elapsed / HOUR)
    } else if elapsed < WEEK {
        (AgeUnit::Days, elapsed / DAY)
    } else if elapsed < 5 * WEEK {
        (AgeUnit::Weeks, elapsed / WEEK)
    } else if elapsed < YEAR {
        (AgeUnit::Months, elapsed / MONTH)
    } else {
        (AgeUnit::Years, elapsed / YEAR)
    };
    CompactAge { unit, count }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_match_the_web_client() {
        let now = 10 * YEAR;
        let age = |elapsed: i64| compact_age(now, now - elapsed);
        assert_eq!(
            age(0),
            CompactAge {
                unit: AgeUnit::Minutes,
                count: 1
            }
        );
        assert_eq!(
            age(-5_000),
            CompactAge {
                unit: AgeUnit::Minutes,
                count: 1
            }
        );
        assert_eq!(
            age(59 * MINUTE + 59_999),
            CompactAge {
                unit: AgeUnit::Minutes,
                count: 59
            }
        );
        assert_eq!(
            age(HOUR),
            CompactAge {
                unit: AgeUnit::Hours,
                count: 1
            }
        );
        assert_eq!(
            age(DAY - 1),
            CompactAge {
                unit: AgeUnit::Hours,
                count: 23
            }
        );
        assert_eq!(
            age(6 * DAY),
            CompactAge {
                unit: AgeUnit::Days,
                count: 6
            }
        );
        assert_eq!(
            age(34 * DAY),
            CompactAge {
                unit: AgeUnit::Weeks,
                count: 4
            }
        );
        assert_eq!(
            age(35 * DAY),
            CompactAge {
                unit: AgeUnit::Months,
                count: 1
            }
        );
        assert_eq!(
            age(YEAR),
            CompactAge {
                unit: AgeUnit::Years,
                count: 1
            }
        );
    }
}
