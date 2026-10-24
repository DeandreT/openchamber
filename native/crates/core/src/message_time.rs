//! A message's time as the turn footer writes it
//! (`formatTimestampForDisplay` in `components/chat/message/timeFormat.ts`):
//! the time today, `Yesterday` and the time, else the short month, the day,
//! the year when it is another one, and the time.
//!
//! The locale's own data (month names, the clock's layout, how each hour and
//! day period reads) is exported from `Intl` by the resource generator, so
//! nothing here knows a locale; callers convert the timestamp to local
//! calendar fields.

/// A calendar day in the user's time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalDate {
    pub year: i32,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
}

/// A moment in the user's time zone, to the minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalDateTime {
    pub date: LocalDate,
    /// 0 to 23.
    pub hour: u32,
    /// 0 to 59.
    pub minute: u32,
}

/// The user's clock choice (`timeFormatPreference`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClockPreference {
    /// The locale's own clock.
    #[default]
    Auto,
    TwelveHour,
    TwentyFourHour,
}

impl ClockPreference {
    /// The settings value: `auto`, `12h` or `24h`; anything else is `Auto`.
    pub fn from_setting(value: &str) -> Self {
        match value {
            "12h" => Self::TwelveHour,
            "24h" => Self::TwentyFourHour,
            _ => Self::Auto,
        }
    }
}

/// One piece of a clock's layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockPart {
    Hour,
    Minute,
    DayPeriod,
    Literal(String),
}

/// How a locale writes the time on one clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockLayout {
    pub parts: Vec<ClockPart>,
    /// The hour as written at each hour of the day, 0 to 23.
    pub hours: Vec<String>,
    /// The day period at each hour of the day; empty on a 24-hour clock.
    pub day_periods: Vec<String>,
}

/// A locale's date and time data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DateFormats {
    /// Whether `Auto` means the 12-hour clock.
    pub twelve_hour_by_default: bool,
    /// Short month names, January first.
    pub months: Vec<String>,
    pub twelve_hour: ClockLayout,
    pub twenty_four_hour: ClockLayout,
}

impl DateFormats {
    /// The time of day, as `formatTimeForPreference` writes it.
    pub fn time(&self, moment: &LocalDateTime, preference: ClockPreference) -> String {
        let twelve = match preference {
            ClockPreference::Auto => self.twelve_hour_by_default,
            ClockPreference::TwelveHour => true,
            ClockPreference::TwentyFourHour => false,
        };
        let clock = if twelve {
            &self.twelve_hour
        } else {
            &self.twenty_four_hour
        };
        let hour = moment.hour as usize;
        let mut text = String::new();
        for part in &clock.parts {
            match part {
                ClockPart::Hour => text.push_str(clock.hours.get(hour).map_or("", String::as_str)),
                ClockPart::Minute => text.push_str(&format!("{:02}", moment.minute)),
                ClockPart::DayPeriod => {
                    text.push_str(clock.day_periods.get(hour).map_or("", String::as_str));
                }
                ClockPart::Literal(literal) => text.push_str(literal),
            }
        }
        text
    }

    /// The footer's time fact. `yesterday_with` writes the time into the
    /// locale's `Yesterday {time}`.
    pub fn message_time(
        &self,
        moment: &LocalDateTime,
        today: LocalDate,
        yesterday: LocalDate,
        preference: ClockPreference,
        yesterday_with: impl FnOnce(&str) -> String,
    ) -> String {
        let time = self.time(moment, preference);
        if moment.date == today {
            return time;
        }
        if moment.date == yesterday {
            return yesterday_with(&time);
        }
        let month = self
            .months
            .get(moment.date.month.saturating_sub(1) as usize)
            .map_or("", String::as_str);
        let date = format!("{month} {}", moment.date.day);
        if moment.date.year == today.year {
            format!("{date}, {time}")
        } else {
            format!("{date}, {}, {time}", moment.date.year)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(parts: &[&str], hours: impl Fn(usize) -> String, periods: bool) -> ClockLayout {
        ClockLayout {
            parts: parts
                .iter()
                .map(|part| match *part {
                    "{hour}" => ClockPart::Hour,
                    "{minute}" => ClockPart::Minute,
                    "{dayPeriod}" => ClockPart::DayPeriod,
                    literal => ClockPart::Literal(literal.to_owned()),
                })
                .collect(),
            hours: (0..24).map(&hours).collect(),
            day_periods: if periods {
                (0..24)
                    .map(|hour| if hour < 12 { "AM" } else { "PM" }.to_owned())
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    /// en-US as `Intl` writes it.
    fn english() -> DateFormats {
        DateFormats {
            twelve_hour_by_default: true,
            months: [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ]
            .map(str::to_owned)
            .to_vec(),
            twelve_hour: layout(
                &["{hour}", ":", "{minute}", " ", "{dayPeriod}"],
                |hour| (if hour % 12 == 0 { 12 } else { hour % 12 }).to_string(),
                true,
            ),
            twenty_four_hour: layout(
                &["{hour}", ":", "{minute}"],
                |hour| format!("{hour:02}"),
                false,
            ),
        }
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> LocalDateTime {
        LocalDateTime {
            date: LocalDate { year, month, day },
            hour,
            minute,
        }
    }

    #[test]
    fn days_read_as_the_web_writes_them() {
        let formats = english();
        let today = LocalDate {
            year: 2026,
            month: 10,
            day: 23,
        };
        let yesterday = LocalDate { day: 22, ..today };
        let write = |moment: LocalDateTime| {
            formats.message_time(&moment, today, yesterday, ClockPreference::Auto, |time| {
                format!("Yesterday {time}")
            })
        };
        assert_eq!(write(at(2026, 10, 23, 0, 5)), "12:05 AM");
        assert_eq!(write(at(2026, 10, 22, 13, 37)), "Yesterday 1:37 PM");
        assert_eq!(write(at(2026, 3, 9, 9, 0)), "Mar 9, 9:00 AM");
        assert_eq!(write(at(2025, 12, 31, 23, 59)), "Dec 31, 2025, 11:59 PM");
    }

    #[test]
    fn the_preference_overrides_the_locale_clock() {
        let formats = english();
        let moment = at(2026, 10, 23, 9, 5);
        assert_eq!(
            formats.time(&moment, ClockPreference::TwentyFourHour),
            "09:05"
        );
        assert_eq!(
            formats.time(&moment, ClockPreference::TwelveHour),
            "9:05 AM"
        );
        assert_eq!(
            ClockPreference::from_setting("24h"),
            ClockPreference::TwentyFourHour
        );
        assert_eq!(
            ClockPreference::from_setting("other"),
            ClockPreference::Auto
        );
    }
}
