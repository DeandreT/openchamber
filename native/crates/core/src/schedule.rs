//! Scheduled tasks: their schedules and the editor's rules.
//!
//! Ports `lib/cron.ts` (cron validation and the next-run preview, which the
//! web takes from `cron-parser`), the list labels of `ScheduledTasksDialog.tsx`
//! and the draft rules of `scheduledTaskEditorDraft.ts`. The server computes
//! and enforces the real schedule; this crate only says what the editor and
//! the list show.

use std::cmp::Ordering;

use crate::time_zone::{self, TimeZone, UnixSeconds};

/// How a task repeats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScheduleKind {
    #[default]
    Daily,
    Weekly,
    Once,
    Cron,
}

impl ScheduleKind {
    pub const ALL: [Self; 4] = [Self::Daily, Self::Weekly, Self::Once, Self::Cron];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Once => "once",
            Self::Cron => "cron",
        }
    }

    /// Any stored kind other than `weekly`, `once` and `cron` reads as daily,
    /// as `toDraft` reads it.
    pub fn parse(value: &str) -> Self {
        match value {
            "weekly" => Self::Weekly,
            "once" => Self::Once,
            "cron" => Self::Cron,
            _ => Self::Daily,
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Daily => "sessions.scheduledTasks.editor.scheduleType.daily",
            Self::Weekly => "sessions.scheduledTasks.editor.scheduleType.weekly",
            Self::Once => "sessions.scheduledTasks.editor.scheduleType.once",
            Self::Cron => "sessions.scheduledTasks.editor.scheduleType.cron",
        }
    }
}

/// A stored schedule as the server reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    pub kind: ScheduleKind,
    pub times: Option<Vec<String>>,
    pub time: Option<String>,
    pub date: Option<String>,
    pub weekdays: Option<Vec<i64>>,
    pub cron: Option<String>,
    pub timezone: Option<String>,
}

/// `HH:MM` on a 24-hour clock.
pub fn is_clock_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && matches!(
            (bytes[0], bytes[1]),
            (b'0' | b'1', b'0'..=b'9') | (b'2', b'0'..=b'3')
        )
        && matches!((bytes[3], bytes[4]), (b'0'..=b'5', b'0'..=b'9'))
}

/// `YYYY-MM-DD` by shape, as the editor checks it.
pub fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

/// `scheduleTimes`: the valid times, deduplicated and sorted.
pub fn schedule_times(schedule: &Schedule) -> Vec<String> {
    let raw: Vec<String> = match (&schedule.times, &schedule.time) {
        (Some(times), _) => times.clone(),
        (None, Some(time)) => vec![time.clone()],
        (None, None) => Vec::new(),
    };
    dedupe_sort_times(raw.iter().map(String::as_str))
}

fn dedupe_sort_times<'a>(times: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut valid: Vec<String> = times
        .into_iter()
        .filter(|time| is_clock_time(time))
        .map(str::to_owned)
        .collect();
    valid.sort();
    valid.dedup();
    valid
}

/// A message key and its placeholder values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub key: &'static str,
    pub params: Vec<(&'static str, String)>,
}

impl Label {
    fn new(key: &'static str) -> Self {
        Self {
            key,
            params: Vec::new(),
        }
    }

    fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.params.push((name, value.into()));
        self
    }
}

/// The short weekday name's key; 0 is Sunday.
pub fn weekday_short_key(weekday: i64) -> &'static str {
    match weekday {
        0 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.sun",
        1 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.mon",
        2 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.tue",
        3 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.wed",
        4 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.thu",
        5 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.fri",
        6 => "sessions.scheduledTasks.dialog.schedule.weekdayShort.sat",
        _ => "sessions.scheduledTasks.dialog.schedule.weekdayShort.unknown",
    }
}

/// `formatSchedule`: the task row's schedule line. Weekday names are the
/// caller's to translate, so a weekly label arrives with `days` already
/// joined by `weekday_name`.
pub fn schedule_label(schedule: &Schedule, weekday_name: impl Fn(i64) -> String) -> Label {
    let zone = schedule
        .timezone
        .as_deref()
        .filter(|zone| !zone.is_empty())
        .map(|zone| time_zone::canonicalize(zone).to_owned());
    let times = schedule_times(schedule);
    let times = if times.is_empty() {
        "--:--".to_owned()
    } else {
        times.join(", ")
    };
    let (plain, zoned) = match schedule.kind {
        ScheduleKind::Daily => (
            "sessions.scheduledTasks.dialog.schedule.daily",
            "sessions.scheduledTasks.dialog.schedule.dailyWithTimezone",
        ),
        ScheduleKind::Weekly => (
            "sessions.scheduledTasks.dialog.schedule.weekly",
            "sessions.scheduledTasks.dialog.schedule.weeklyWithTimezone",
        ),
        ScheduleKind::Once => (
            "sessions.scheduledTasks.dialog.schedule.once",
            "sessions.scheduledTasks.dialog.schedule.onceWithTimezone",
        ),
        ScheduleKind::Cron => (
            "sessions.scheduledTasks.dialog.schedule.cron",
            "sessions.scheduledTasks.dialog.schedule.cronWithTimezone",
        ),
    };
    let mut label = Label::new(if zone.is_some() { zoned } else { plain });
    label = match schedule.kind {
        ScheduleKind::Daily => label.with("time", times),
        ScheduleKind::Weekly => {
            let days = schedule
                .weekdays
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|day| weekday_name(*day))
                .collect::<Vec<_>>()
                .join(", ");
            label.with("days", days).with("time", times)
        }
        ScheduleKind::Once => {
            let date = schedule
                .date
                .as_deref()
                .map(str::trim)
                .filter(|date| !date.is_empty());
            let time = schedule
                .time
                .as_deref()
                .map(str::trim)
                .filter(|time| !time.is_empty())
                .unwrap_or("--:--");
            // An unknown date is a translated phrase, marked for the caller.
            label
                .with("date", date.unwrap_or(UNKNOWN_DATE))
                .with("time", time)
        }
        ScheduleKind::Cron => label.with("cron", schedule.cron.clone().unwrap_or_default()),
    };
    if let Some(zone) = zone {
        label = label.with("timezone", zone);
    }
    label
}

/// Stands in a once label's `date` for `schedule.unknownDate`.
pub const UNKNOWN_DATE: &str = "\u{0}unknown-date";

/// `formatRelativeTime` of the task list: "in 5 min", "2h 10m ago".
pub fn relative_label(at_ms: i64, now_ms: i64) -> Label {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let diff = at_ms - now_ms;
    let abs = diff.abs();
    let future = diff >= 0;
    let rounded = |value: i64, unit: i64| (value + unit / 2) / unit;
    if abs < MINUTE {
        return Label::new(if future {
            "sessions.scheduledTasks.dialog.relativeTime.inLessThanOneMinute"
        } else {
            "sessions.scheduledTasks.dialog.relativeTime.justNow"
        });
    }
    if abs < HOUR {
        let minutes = rounded(abs, MINUTE).to_string();
        return Label::new(if future {
            "sessions.scheduledTasks.dialog.relativeTime.inMinutes"
        } else {
            "sessions.scheduledTasks.dialog.relativeTime.minutesAgo"
        })
        .with("count", minutes);
    }
    let duration = if abs < DAY {
        let hours = abs / HOUR;
        let minutes = rounded(abs % HOUR, MINUTE);
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        let days = abs / DAY;
        let hours = rounded(abs % DAY, HOUR);
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    };
    Label::new(if future {
        "sessions.scheduledTasks.dialog.relativeTime.inDuration"
    } else {
        "sessions.scheduledTasks.dialog.relativeTime.durationAgo"
    })
    .with("duration", duration)
}

/// A task's last run status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RunStatus {
    #[default]
    Idle,
    Running,
    Success,
    Error,
    Queued,
    Sent,
    Skipped,
    Failed,
    Cancelled,
}

/// How a status is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusTone {
    Success,
    Error,
    Warning,
    Muted,
}

impl RunStatus {
    /// An unknown status reads as idle.
    pub fn parse(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "success" => Self::Success,
            "error" => Self::Error,
            "queued" => Self::Queued,
            "sent" => Self::Sent,
            "skipped" => Self::Skipped,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Idle,
        }
    }

    /// `STATUS_META`: tone and icon.
    pub fn look(self) -> (StatusTone, &'static str) {
        match self {
            Self::Success | Self::Sent => (StatusTone::Success, "checkbox-circle"),
            Self::Error | Self::Failed => (StatusTone::Error, "error-warning"),
            Self::Running => (StatusTone::Warning, "loader-4"),
            Self::Queued => (StatusTone::Warning, "pulse"),
            Self::Idle | Self::Skipped | Self::Cancelled => (StatusTone::Muted, "pulse"),
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Queued => "sessions.scheduledTasks.dialog.status.queued",
            Self::Sent => "sessions.scheduledTasks.dialog.status.sent",
            Self::Skipped => "sessions.scheduledTasks.dialog.status.skipped",
            Self::Failed | Self::Error => "sessions.scheduledTasks.dialog.status.error",
            Self::Cancelled => "sessions.scheduledTasks.dialog.status.cancelled",
            Self::Success => "sessions.scheduledTasks.dialog.status.success",
            Self::Running => "sessions.scheduledTasks.dialog.status.running",
            Self::Idle => "sessions.scheduledTasks.dialog.status.idle",
        }
    }
}

/// The list order: enabled first, then by name (`localeCompare` for the
/// ASCII names tasks carry), then the sooner next run.
pub fn compare_tasks(a: (bool, &str, Option<i64>), b: (bool, &str, Option<i64>)) -> Ordering {
    b.0.cmp(&a.0)
        .then_with(|| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then_with(|| b.1.cmp(a.1))
        })
        .then_with(|| a.2.unwrap_or(i64::MAX).cmp(&b.2.unwrap_or(i64::MAX)))
}

/// Whether a new run goes to a new session or an existing one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SessionMode {
    #[default]
    New,
    Existing,
}

/// The editor's form (`ScheduledTaskDraft`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskDraft {
    pub id: Option<String>,
    pub session_mode: SessionMode,
    pub target_session_id: String,
    pub name: String,
    pub enabled: bool,
    pub kind: ScheduleKind,
    pub times: Vec<String>,
    pub once_date: String,
    pub once_time: String,
    pub weekdays: Vec<i64>,
    pub timezone: String,
    pub cron: String,
    pub prompt: String,
    pub provider_id: String,
    pub model_id: String,
    pub variant: String,
    pub agent: String,
    pub use_defaults: bool,
    pub goal_enabled: bool,
    pub goal_token_budget: Option<i64>,
    pub permission_auto_accept: bool,
    pub run_in_worktree: bool,
    pub base_branch: String,
    pub cleanup_worktree: bool,
}

/// The model, variant and agent a new task starts from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftDefaults {
    pub provider_id: String,
    pub model_id: String,
    pub variant: String,
    pub agent: String,
    /// The local zone, already canonical.
    pub timezone: String,
    /// Today's local date as `YYYY-MM-DD`.
    pub today: String,
}

impl TaskDraft {
    /// `toDraft(null, …)`: a new daily task at 09:00.
    pub fn new_task(defaults: &DraftDefaults) -> Self {
        Self {
            enabled: true,
            kind: ScheduleKind::Daily,
            times: vec!["09:00".to_owned()],
            once_date: defaults.today.clone(),
            once_time: "09:00".to_owned(),
            weekdays: vec![1],
            timezone: defaults.timezone.clone(),
            provider_id: defaults.provider_id.clone(),
            model_id: defaults.model_id.clone(),
            variant: defaults.variant.clone(),
            agent: defaults.agent.clone(),
            cleanup_worktree: true,
            ..Self::default()
        }
    }

    /// Adds a time row at noon.
    pub fn add_time(&mut self) {
        self.times.push("12:00".to_owned());
    }

    /// Removes a time row; the last one left becomes 09:00.
    pub fn remove_time(&mut self, index: usize) {
        if index < self.times.len() {
            self.times.remove(index);
        }
        if self.times.is_empty() {
            self.times.push("09:00".to_owned());
        }
    }

    pub fn toggle_weekday(&mut self, weekday: i64) {
        if let Some(index) = self.weekdays.iter().position(|day| *day == weekday) {
            self.weekdays.remove(index);
        } else {
            self.weekdays.push(weekday);
            self.weekdays.sort_unstable();
        }
    }

    /// Picks a schedule kind; cron starts at the top of each hour when empty.
    pub fn set_kind(&mut self, kind: ScheduleKind) {
        self.kind = kind;
        if kind == ScheduleKind::Cron && self.cron.is_empty() {
            self.cron = "0 * * * *".to_owned();
        }
    }

    /// The times a save sends: valid, deduplicated and sorted.
    pub fn saved_times(&self) -> Vec<String> {
        dedupe_sort_times(self.times.iter().map(String::as_str))
    }

    /// `validateDraft`: the first problem's message key, if any.
    pub fn validate(&self) -> Option<&'static str> {
        if self.session_mode == SessionMode::Existing && self.target_session_id.is_empty() {
            return Some("sessions.scheduledTasks.editor.targetRequired");
        }
        if self.name.trim().is_empty() {
            return Some("sessions.scheduledTasks.editor.validation.taskNameRequired");
        }
        if self.prompt.trim().is_empty() {
            return Some("sessions.scheduledTasks.editor.validation.promptRequired");
        }
        if !self.use_defaults
            && (self.provider_id.trim().is_empty() || self.model_id.trim().is_empty())
        {
            return Some("sessions.scheduledTasks.editor.validation.modelRequired");
        }
        match self.kind {
            ScheduleKind::Once => {
                if !is_iso_date(&self.once_date) {
                    return Some("sessions.scheduledTasks.editor.validation.dateFormat");
                }
                if !is_clock_time(&self.once_time) {
                    return Some("sessions.scheduledTasks.editor.validation.timeFormat");
                }
            }
            ScheduleKind::Cron => {
                if self.cron.trim().is_empty() {
                    return Some("sessions.scheduledTasks.editor.validation.cronRequired");
                }
                if CronExpression::parse(&self.cron).is_err() {
                    return Some("sessions.scheduledTasks.editor.validation.cronInvalid");
                }
            }
            ScheduleKind::Daily | ScheduleKind::Weekly => {
                if !self.times.iter().any(|time| is_clock_time(time)) {
                    return Some("sessions.scheduledTasks.editor.validation.atLeastOneTime");
                }
            }
        }
        if self.kind == ScheduleKind::Weekly && self.weekdays.is_empty() {
            return Some("sessions.scheduledTasks.editor.validation.atLeastOneWeekday");
        }
        if self.timezone.trim().is_empty() {
            return Some("sessions.scheduledTasks.editor.validation.timezoneRequired");
        }
        None
    }
}

/// The cron examples the editor offers (`CRON_EXAMPLES`).
pub const CRON_EXAMPLES: &[(&str, &str)] = &[
    (
        "*/5 * * * *",
        "sessions.scheduledTasks.editor.cronExpression.examples.every5min",
    ),
    (
        "0 * * * *",
        "sessions.scheduledTasks.editor.cronExpression.examples.everyHour",
    ),
    (
        "0 9 * * 1",
        "sessions.scheduledTasks.editor.cronExpression.examples.monday9am",
    ),
    (
        "0 9,17 * * *",
        "sessions.scheduledTasks.editor.cronExpression.examples.9am5pm",
    ),
    (
        "0 0 1 * *",
        "sessions.scheduledTasks.editor.cronExpression.examples.firstOfMonth",
    ),
];

/// Why a cron expression does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronError {
    Empty,
    FieldCount(usize),
    Field(&'static str),
}

/// A set of values 0 to 63.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Bits(u64);

impl Bits {
    fn set(&mut self, value: u32) {
        self.0 |= 1 << value;
    }

    fn has(self, value: u32) -> bool {
        self.0 & (1 << value) != 0
    }

    fn values(self, range: std::ops::RangeInclusive<u32>) -> impl Iterator<Item = u32> {
        range.filter(move |value| self.has(*value))
    }
}

/// A parsed five- or six-field cron expression, the forms `cron-parser`
/// accepts: lists, ranges, steps, month and weekday names, `?`, `L`
/// (last day, or last given weekday) and `#` (nth weekday), plus the
/// `@yearly`, `@monthly`, `@weekly`, `@daily` and `@hourly` shorthands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpression {
    seconds: Bits,
    minutes: Bits,
    hours: Bits,
    days: Bits,
    last_day: bool,
    months: Bits,
    weekdays: Bits,
    /// `d#n`: weekday and week number.
    nth_weekdays: Vec<(u32, u32)>,
    /// `dL`: the last such weekday of the month.
    last_weekdays: Bits,
    day_restricted: bool,
    weekday_restricted: bool,
}

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

impl CronExpression {
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let expression = expression.trim();
        if expression.is_empty() {
            return Err(CronError::Empty);
        }
        let expanded = match expression.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 0 1 1 *".to_owned(),
            "@monthly" => "0 0 0 1 * *".to_owned(),
            "@weekly" => "0 0 0 * * 0".to_owned(),
            "@daily" | "@midnight" => "0 0 0 * * *".to_owned(),
            "@hourly" => "0 0 * * * *".to_owned(),
            _ => expression.to_owned(),
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let (second, rest) = match fields.len() {
            5 => ("0", &fields[..]),
            6 => (fields[0], &fields[1..]),
            count => return Err(CronError::FieldCount(count)),
        };
        let seconds = plain_field(second, 0, 59, &[], "second")?;
        let minutes = plain_field(rest[0], 0, 59, &[], "minute")?;
        let hours = plain_field(rest[1], 0, 23, &[], "hour")?;
        let months = plain_field(rest[3], 1, 12, &MONTH_NAMES, "month")?;
        let (days, last_day) = day_field(rest[2])?;
        let (weekdays, nth_weekdays, last_weekdays) = weekday_field(rest[4])?;
        Ok(Self {
            seconds,
            minutes,
            hours,
            days,
            last_day,
            months,
            weekdays,
            nth_weekdays,
            last_weekdays,
            day_restricted: !is_wildcard(rest[2]),
            weekday_restricted: !is_wildcard(rest[4]),
        })
    }

    /// Whether the expression fires on a date. A restricted day of month and
    /// a restricted weekday are either-or, as in classic cron.
    fn matches_date(&self, year: i64, month: u32, day: u32, weekday: u32) -> bool {
        if !self.months.has(month) {
            return false;
        }
        let length = time_zone::days_in_month(year, month);
        let by_day = self.days.has(day) || (self.last_day && day == length);
        let week = (day - 1) / 7 + 1;
        let by_weekday = self.weekdays.has(weekday)
            || self.nth_weekdays.contains(&(weekday, week))
            || (self.last_weekdays.has(weekday) && day + 7 > length);
        match (self.day_restricted, self.weekday_restricted) {
            (true, true) => by_day || by_weekday,
            (true, false) => by_day,
            (false, true) => by_weekday,
            (false, false) => true,
        }
    }

    /// The next `count` firing instants strictly after `after`, in seconds,
    /// read on `zone`'s wall clock. A wall time a forward clock change skips
    /// does not fire; one that repeats fires once. Searches eight years.
    pub fn next_runs(&self, zone: &TimeZone, after: UnixSeconds, count: usize) -> Vec<UnixSeconds> {
        let mut runs = Vec::new();
        if count == 0 {
            return runs;
        }
        let local_after = after + i64::from(zone.offset_at(after));
        let first_day = local_after.div_euclid(86_400) - 1;
        for day in first_day..first_day + 366 * 8 {
            let (year, month, date) = time_zone::civil_from_days(day);
            let weekday = time_zone::weekday_of(day) as u32;
            if !self.matches_date(year, month, date, weekday) {
                continue;
            }
            for hour in self.hours.values(0..=23) {
                for minute in self.minutes.values(0..=59) {
                    for second in self.seconds.values(0..=59) {
                        let local = day * 86_400
                            + i64::from(hour) * 3600
                            + i64::from(minute) * 60
                            + i64::from(second);
                        if local + 86_400 < local_after {
                            continue;
                        }
                        let Some(instant) = zone.to_instant(local) else {
                            continue;
                        };
                        if instant > after && runs.last().is_none_or(|last| instant > *last) {
                            runs.push(instant);
                            if runs.len() == count {
                                return runs;
                            }
                        }
                    }
                }
            }
        }
        runs
    }
}

fn is_wildcard(field: &str) -> bool {
    field == "*" || field == "?"
}

/// One field of numbers, names, ranges, lists and steps.
fn plain_field(
    field: &str,
    min: u32,
    max: u32,
    names: &[&str],
    which: &'static str,
) -> Result<Bits, CronError> {
    let mut bits = Bits::default();
    for part in field.split(',') {
        for value in expand(part, min, max, names).ok_or(CronError::Field(which))? {
            bits.set(value);
        }
    }
    Ok(bits)
}

/// Values of one list part: `*`, `?`, `n`, `a-b`, each optionally `/step`,
/// where a single start with a step runs to the field's end.
fn expand(part: &str, min: u32, max: u32, names: &[&str]) -> Option<Vec<u32>> {
    let (range, step) = match part.split_once('/') {
        Some((range, step)) => (
            range,
            Some(step.parse::<u32>().ok().filter(|step| *step > 0)?),
        ),
        None => (part, None),
    };
    let value = |text: &str| -> Option<u32> {
        let parsed = text.parse::<u32>().ok().or_else(|| {
            names
                .iter()
                .position(|name| name.eq_ignore_ascii_case(text))
                .map(|index| index as u32 + min)
        })?;
        (min..=max).contains(&parsed).then_some(parsed)
    };
    let (start, end) = if range == "*" || range == "?" {
        (min, max)
    } else if let Some((start, end)) = range.split_once('-') {
        let (start, end) = (value(start)?, value(end)?);
        if start > end {
            return None;
        }
        (start, end)
    } else {
        let single = value(range)?;
        (single, if step.is_some() { max } else { single })
    };
    Some((start..=end).step_by(step.unwrap_or(1) as usize).collect())
}

fn day_field(field: &str) -> Result<(Bits, bool), CronError> {
    let mut bits = Bits::default();
    let mut last = false;
    for part in field.split(',') {
        if part.eq_ignore_ascii_case("L") {
            last = true;
            continue;
        }
        for value in expand(part, 1, 31, &[]).ok_or(CronError::Field("day of month"))? {
            bits.set(value);
        }
    }
    Ok((bits, last))
}

type WeekdayField = (Bits, Vec<(u32, u32)>, Bits);

fn weekday_field(field: &str) -> Result<WeekdayField, CronError> {
    let invalid = || CronError::Field("day of week");
    let weekday = |text: &str| -> Option<u32> {
        let value = text.parse::<u32>().ok().or_else(|| {
            WEEKDAY_NAMES
                .iter()
                .position(|name| name.eq_ignore_ascii_case(text))
                .map(|index| index as u32)
        })?;
        (value <= 7).then_some(value % 7)
    };
    let mut bits = Bits::default();
    let mut nth = Vec::new();
    let mut last = Bits::default();
    for part in field.split(',') {
        if let Some((day, week)) = part.split_once('#') {
            let week = week
                .parse::<u32>()
                .ok()
                .filter(|week| (1..=5).contains(week))
                .ok_or_else(invalid)?;
            nth.push((weekday(day).ok_or_else(invalid)?, week));
            continue;
        }
        if let Some(day) = part
            .strip_suffix('L')
            .or_else(|| part.strip_suffix('l'))
            .filter(|day| !day.is_empty())
        {
            last.set(weekday(day).ok_or_else(invalid)?);
            continue;
        }
        // Seven is Sunday too, so the range runs to 7 and folds onto 0.
        for value in expand(part, 0, 7, &WEEKDAY_NAMES).ok_or_else(invalid)? {
            bits.set(value % 7);
        }
    }
    Ok((bits, nth, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_zone::days_from_civil;

    fn at(year: i64, month: u32, day: u32, hour: i64, minute: i64) -> i64 {
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60
    }

    #[test]
    fn cron_accepts_what_cron_parser_accepts() {
        for valid in [
            "*/5 * * * *",
            "0 9,17 * * *",
            "0 0 1 * *",
            "0 9 * * MON-FRI",
            "30 0 9 ? JAN,jul 1",
            "0 0 L * *",
            "0 12 * * 5L",
            "0 12 * * 1#2",
            "0 0 * * 7",
            "5/15 * * * *",
            "@daily",
        ] {
            assert!(CronExpression::parse(valid).is_ok(), "{valid} parses");
        }
        for invalid in [
            "",
            "* * * *",
            "* * * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "nope * * * *",
            "0 12 * * 1#6",
        ] {
            assert!(CronExpression::parse(invalid).is_err(), "{invalid:?} fails");
        }
    }

    #[test]
    fn next_runs_follow_the_wall_clock_of_the_zone() {
        let utc = TimeZone::utc();
        let cron = CronExpression::parse("0 9 * * 1").expect("parses");
        // 2026-11-07 is a Saturday; the next Mondays are the 9th and 16th.
        let runs = cron.next_runs(&utc, at(2026, 11, 7, 12, 0), 2);
        assert_eq!(runs, [at(2026, 11, 9, 9, 0), at(2026, 11, 16, 9, 0)]);
        let every_five = CronExpression::parse("*/5 * * * *").expect("parses");
        assert_eq!(
            every_five.next_runs(&utc, at(2026, 1, 1, 0, 5), 2),
            [at(2026, 1, 1, 0, 10), at(2026, 1, 1, 0, 15)],
            "a run exactly now is not next"
        );
        let last = CronExpression::parse("0 0 L 2 *").expect("parses");
        assert_eq!(
            last.next_runs(&utc, at(2027, 3, 1, 0, 0), 1),
            [at(2028, 2, 29, 0, 0)]
        );
        let nth = CronExpression::parse("0 0 * * 2#1").expect("parses");
        assert_eq!(
            nth.next_runs(&utc, at(2026, 11, 7, 0, 0), 1),
            [at(2026, 12, 1, 0, 0)]
        );
    }

    #[test]
    fn day_and_weekday_restrictions_are_either_or() {
        let cron = CronExpression::parse("0 0 13 * 5").expect("parses");
        // 2026-11-13 is a Friday and the 13th; Friday the 6th fires too.
        let runs = cron.next_runs(&TimeZone::utc(), at(2026, 11, 1, 0, 0), 3);
        assert_eq!(
            runs,
            [
                at(2026, 11, 6, 0, 0),
                at(2026, 11, 13, 0, 0),
                at(2026, 11, 20, 0, 0)
            ]
        );
    }

    #[test]
    fn schedule_labels_name_their_times_days_and_zone() {
        let weekly = Schedule {
            kind: ScheduleKind::Weekly,
            times: Some(vec![
                "17:00".into(),
                "09:00".into(),
                "9:00".into(),
                "09:00".into(),
            ]),
            weekdays: Some(vec![1, 3]),
            timezone: Some("Europe/Kiev".into()),
            ..Schedule::default()
        };
        let label = schedule_label(&weekly, |day| format!("d{day}"));
        assert_eq!(
            label.key,
            "sessions.scheduledTasks.dialog.schedule.weeklyWithTimezone"
        );
        assert_eq!(
            label.params,
            [
                ("days", "d1, d3".to_owned()),
                ("time", "09:00, 17:00".to_owned()),
                ("timezone", "Europe/Kyiv".to_owned()),
            ]
        );
        let once = Schedule {
            kind: ScheduleKind::Once,
            ..Schedule::default()
        };
        let label = schedule_label(&once, |_| String::new());
        assert_eq!(label.key, "sessions.scheduledTasks.dialog.schedule.once");
        assert_eq!(
            label.params,
            [
                ("date", UNKNOWN_DATE.to_owned()),
                ("time", "--:--".to_owned())
            ]
        );
    }

    #[test]
    fn relative_labels_round_like_the_list() {
        let now = 1_000_000_000;
        assert_eq!(
            relative_label(now + 30_000, now).key,
            "sessions.scheduledTasks.dialog.relativeTime.inLessThanOneMinute"
        );
        assert_eq!(
            relative_label(now - 5 * 60_000, now).params,
            [("count", "5".to_owned())]
        );
        assert_eq!(
            relative_label(now + 2 * 3_600_000 + 10 * 60_000, now).params,
            [("duration", "2h 10m".to_owned())]
        );
        assert_eq!(
            relative_label(now - 3 * 86_400_000, now).params,
            [("duration", "3d".to_owned())]
        );
    }

    #[test]
    fn drafts_report_the_first_problem() {
        let defaults = DraftDefaults {
            provider_id: "p".into(),
            model_id: "m".into(),
            timezone: "UTC".into(),
            today: "2026-11-07".into(),
            ..DraftDefaults::default()
        };
        let mut draft = TaskDraft::new_task(&defaults);
        assert_eq!(
            draft.validate(),
            Some("sessions.scheduledTasks.editor.validation.taskNameRequired")
        );
        draft.name = "Daily digest".into();
        draft.prompt = "Summarize".into();
        assert_eq!(draft.validate(), None);
        draft.set_kind(ScheduleKind::Cron);
        assert_eq!(draft.cron, "0 * * * *");
        draft.cron = "61 * * * *".into();
        assert_eq!(
            draft.validate(),
            Some("sessions.scheduledTasks.editor.validation.cronInvalid")
        );
        draft.set_kind(ScheduleKind::Weekly);
        draft.toggle_weekday(1);
        assert_eq!(
            draft.validate(),
            Some("sessions.scheduledTasks.editor.validation.atLeastOneWeekday")
        );
        draft.session_mode = SessionMode::Existing;
        assert_eq!(
            draft.validate(),
            Some("sessions.scheduledTasks.editor.targetRequired")
        );
        draft.remove_time(0);
        assert_eq!(draft.times, ["09:00"], "the last time row stays");
    }

    #[test]
    fn tasks_sort_enabled_then_by_name_then_next_run() {
        let mut tasks = [
            (false, "a", None),
            (true, "beta", Some(5)),
            (true, "Alpha", Some(9)),
            (true, "alpha", Some(1)),
        ];
        tasks.sort_by(|a, b| compare_tasks(*a, *b));
        assert_eq!(
            tasks.iter().map(|task| task.1).collect::<Vec<_>>(),
            ["alpha", "Alpha", "beta", "a"]
        );
    }
}
