use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use chrono::{Datelike, Timelike, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Time from `now` until the next wall-clock multiple of `period`.
fn delay_to_next_boundary(now: SystemTime, period: Duration) -> Duration {
    let period_ns = period.as_nanos();
    if period_ns == 0 {
        return Duration::ZERO;
    }
    let since_epoch = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    match since_epoch % period_ns {
        0 => Duration::ZERO,
        remainder => Duration::from_nanos(u64::try_from(period_ns - remainder).unwrap_or(0)),
    }
}

/// A `period`-spaced interval whose ticks land on wall-clock boundaries.
///
/// `tokio::time::interval` counts from whenever the task was spawned, so a 30s
/// tick started at 21:43:46.166 fires at :16.166 and :46.166 past every minute
/// for the life of the process. Time-of-day tasks polling on top of that then
/// act at a fixed but arbitrary offset into their target minute — a daemon
/// started at 21:43:46.166 ran lights-out at 01:00:16.168 every night. Anchor
/// the first tick to the next boundary so the offset comes from the clock
/// rather than from the last restart.
pub fn aligned_interval(period: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(
        tokio::time::Instant::now() + delay_to_next_boundary(SystemTime::now(), period),
        period,
    )
}

/// Validated "HH:MM" time representation stored as minutes since midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOfDay(u16);

impl TimeOfDay {
    pub fn from_hm_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 2 {
            bail!("Invalid time '{}': expected HH:MM format", s);
        }
        let hour: u16 = parts[0]
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid hour in time '{}'", s))?;
        let minute: u16 = parts[1]
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid minute in time '{}'", s))?;
        if hour > 23 || minute > 59 {
            bail!("Invalid time '{}': hour must be 0-23, minute 0-59", s);
        }
        Ok(Self(hour * 60 + minute))
    }

    pub fn as_minutes(self) -> u16 {
        self.0
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

impl Serialize for TimeOfDay {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        TimeOfDay::from_hm_str(&s).map_err(de::Error::custom)
    }
}

impl JsonSchema for TimeOfDay {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("TimeOfDay")
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as JsonSchema>::json_schema(generator)
    }
}

/// Day of week for schedule filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DayOfWeek {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl DayOfWeek {
    fn from_chrono(wd: chrono::Weekday) -> Self {
        match wd {
            chrono::Weekday::Mon => Self::Mon,
            chrono::Weekday::Tue => Self::Tue,
            chrono::Weekday::Wed => Self::Wed,
            chrono::Weekday::Thu => Self::Thu,
            chrono::Weekday::Fri => Self::Fri,
            chrono::Weekday::Sat => Self::Sat,
            chrono::Weekday::Sun => Self::Sun,
        }
    }
}

/// Month for schedule filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Month {
    Jan,
    Feb,
    Mar,
    Apr,
    May,
    Jun,
    Jul,
    Aug,
    Sep,
    Oct,
    Nov,
    Dec,
}

impl Month {
    fn from_chrono_month(m: u32) -> Self {
        match m {
            1 => Self::Jan,
            2 => Self::Feb,
            3 => Self::Mar,
            4 => Self::Apr,
            5 => Self::May,
            6 => Self::Jun,
            7 => Self::Jul,
            8 => Self::Aug,
            9 => Self::Sep,
            10 => Self::Oct,
            11 => Self::Nov,
            12 => Self::Dec,
            _ => Self::Jan,
        }
    }
}

/// A time window predicate. All present fields are ANDed. Empty filters match everything.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimeWindow {
    pub after: Option<TimeOfDay>,
    pub before: Option<TimeOfDay>,
    #[serde(default)]
    pub days: Vec<DayOfWeek>,
    #[serde(default)]
    pub months: Vec<Month>,
}

impl TimeWindow {
    pub fn matches(&self, now: &LocalNow) -> bool {
        // Check time range
        if let (Some(after), Some(before)) = (self.after, self.before) {
            let a = after.as_minutes() as u32;
            let b = before.as_minutes() as u32;
            let n = now.minutes as u32;
            let in_range = if a <= b {
                n >= a && n < b
            } else {
                // Midnight crossover
                n >= a || n < b
            };
            if !in_range {
                return false;
            }
        } else if let Some(after) = self.after {
            if (now.minutes as u32) < after.as_minutes() as u32 {
                return false;
            }
        } else if let Some(before) = self.before
            && (now.minutes as u32) >= before.as_minutes() as u32
        {
            return false;
        }

        // Check day filter
        if !self.days.is_empty() && !self.days.contains(&now.weekday) {
            return false;
        }

        // Check month filter
        if !self.months.is_empty() && !self.months.contains(&now.month) {
            return false;
        }

        true
    }
}

/// Composable temporal predicate.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Schedule {
    /// Composite: all predicates must match.
    All { all: Vec<Schedule> },
    /// Composite: any predicate must match.
    Any { any: Vec<Schedule> },
    /// Composite: invert result.
    Not { not: Box<Schedule> },
    /// A flat time window (the common case).
    Window(TimeWindow),
}

impl Schedule {
    pub fn matches(&self, now: &LocalNow) -> bool {
        match self {
            Schedule::Window(w) => w.matches(now),
            Schedule::All { all } => all.iter().all(|s| s.matches(now)),
            Schedule::Any { any } => any.iter().any(|s| s.matches(now)),
            Schedule::Not { not } => !not.matches(now),
        }
    }
}

/// Snapshot of the current local time for schedule evaluation.
#[derive(Debug, Clone)]
pub struct LocalNow {
    pub minutes: u16,
    pub weekday: DayOfWeek,
    pub month: Month,
}

impl LocalNow {
    pub fn now(timezone: &str) -> Self {
        let now = Utc::now();
        let tz: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
        let local = now.with_timezone(&tz);
        Self {
            minutes: (local.hour() * 60 + local.minute()) as u16,
            weekday: DayOfWeek::from_chrono(local.weekday()),
            month: Month::from_chrono_month(local.month()),
        }
    }
}

/// Validate a Schedule recursively (ensures all TimeOfDay values are valid).
/// Since TimeOfDay validates on deserialize, this mainly checks structural issues.
pub fn validate_schedule(schedule: &Schedule, field: &str) -> Result<()> {
    match schedule {
        Schedule::Window(w) => {
            // TimeOfDay values are already validated on parse; nothing extra needed
            if w.after.is_none() && w.before.is_none() && w.days.is_empty() && w.months.is_empty() {
                // Valid: matches everything (no-op window)
            }
            let _ = (w, field);
            Ok(())
        }
        Schedule::All { all } => {
            if all.is_empty() {
                bail!("Empty 'all' schedule in {}", field);
            }
            for (i, s) in all.iter().enumerate() {
                validate_schedule(s, &format!("{}.all[{}]", field, i))?;
            }
            Ok(())
        }
        Schedule::Any { any } => {
            if any.is_empty() {
                bail!("Empty 'any' schedule in {}", field);
            }
            for (i, s) in any.iter().enumerate() {
                validate_schedule(s, &format!("{}.any[{}]", field, i))?;
            }
            Ok(())
        }
        Schedule::Not { not } => validate_schedule(not, &format!("{}.not", field)),
    }
}

impl FromStr for TimeOfDay {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::from_hm_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_now(minutes: u16, weekday: DayOfWeek, month: Month) -> LocalNow {
        LocalNow {
            minutes,
            weekday,
            month,
        }
    }

    #[test]
    fn test_delay_to_next_boundary_aligns_to_wall_clock() {
        let period = Duration::from_secs(30);

        // 21:43:46.166875 past the epoch — the start time that made the
        // production daemon fire lights-out at 01:00:16.168 every night.
        let offset = Duration::from_secs(21 * 3600 + 43 * 60 + 46) + Duration::from_micros(166_875);
        let delay = delay_to_next_boundary(UNIX_EPOCH + offset, period);
        assert_eq!(
            delay,
            Duration::from_secs(13) + Duration::from_micros(833_125)
        );

        // Landing exactly on a boundary must not wait a whole extra period.
        assert_eq!(
            delay_to_next_boundary(UNIX_EPOCH + Duration::from_secs(60), period),
            Duration::ZERO
        );

        // Every result lands the next tick on a multiple of the period.
        for micros in [1u64, 999_999, 15_000_000, 29_999_999] {
            let now = UNIX_EPOCH + Duration::from_secs(1_000_000) + Duration::from_micros(micros);
            let delay = delay_to_next_boundary(now, period);
            let next = (now + delay).duration_since(UNIX_EPOCH).unwrap();
            assert!(delay < period, "delay {delay:?} should be under one period");
            assert_eq!(next.as_nanos() % period.as_nanos(), 0);
        }
    }

    #[test]
    fn test_delay_to_next_boundary_zero_period_does_not_panic() {
        assert_eq!(
            delay_to_next_boundary(UNIX_EPOCH + Duration::from_secs(5), Duration::ZERO),
            Duration::ZERO
        );
    }

    #[test]
    fn test_time_of_day_parse() {
        assert_eq!(TimeOfDay::from_hm_str("06:00").unwrap().as_minutes(), 360);
        assert_eq!(TimeOfDay::from_hm_str("23:59").unwrap().as_minutes(), 1439);
        assert_eq!(TimeOfDay::from_hm_str("00:00").unwrap().as_minutes(), 0);
        assert!(TimeOfDay::from_hm_str("24:00").is_err());
        assert!(TimeOfDay::from_hm_str("12:60").is_err());
        assert!(TimeOfDay::from_hm_str("noon").is_err());
    }

    #[test]
    fn test_time_of_day_display() {
        assert_eq!(
            TimeOfDay::from_hm_str("06:00").unwrap().to_string(),
            "06:00"
        );
        assert_eq!(
            TimeOfDay::from_hm_str("23:59").unwrap().to_string(),
            "23:59"
        );
    }

    #[test]
    fn test_time_window_normal_range() {
        let w = TimeWindow {
            after: Some(TimeOfDay::from_hm_str("09:00").unwrap()),
            before: Some(TimeOfDay::from_hm_str("17:00").unwrap()),
            days: vec![],
            months: vec![],
        };
        // 10:00 in range
        assert!(w.matches(&make_now(600, DayOfWeek::Mon, Month::Jan)));
        // 08:00 not in range
        assert!(!w.matches(&make_now(480, DayOfWeek::Mon, Month::Jan)));
        // 17:00 not in range (before is exclusive)
        assert!(!w.matches(&make_now(1020, DayOfWeek::Mon, Month::Jan)));
    }

    #[test]
    fn test_time_window_midnight_crossover() {
        let w = TimeWindow {
            after: Some(TimeOfDay::from_hm_str("22:00").unwrap()),
            before: Some(TimeOfDay::from_hm_str("06:30").unwrap()),
            days: vec![],
            months: vec![],
        };
        // 23:20 in range
        assert!(w.matches(&make_now(23 * 60 + 20, DayOfWeek::Mon, Month::Jan)));
        // 02:00 in range
        assert!(w.matches(&make_now(120, DayOfWeek::Mon, Month::Jan)));
        // 12:00 not in range
        assert!(!w.matches(&make_now(720, DayOfWeek::Mon, Month::Jan)));
        // 06:30 not in range (exclusive)
        assert!(!w.matches(&make_now(390, DayOfWeek::Mon, Month::Jan)));
    }

    #[test]
    fn test_time_window_day_filter() {
        let w = TimeWindow {
            after: Some(TimeOfDay::from_hm_str("08:00").unwrap()),
            before: Some(TimeOfDay::from_hm_str("17:00").unwrap()),
            days: vec![
                DayOfWeek::Mon,
                DayOfWeek::Tue,
                DayOfWeek::Wed,
                DayOfWeek::Thu,
                DayOfWeek::Fri,
            ],
            months: vec![],
        };
        // Weekday, in time range
        assert!(w.matches(&make_now(600, DayOfWeek::Wed, Month::Mar)));
        // Weekend, in time range
        assert!(!w.matches(&make_now(600, DayOfWeek::Sat, Month::Mar)));
    }

    #[test]
    fn test_time_window_month_filter() {
        let w = TimeWindow {
            after: Some(TimeOfDay::from_hm_str("06:00").unwrap()),
            before: Some(TimeOfDay::from_hm_str("09:00").unwrap()),
            days: vec![],
            months: vec![Month::Oct, Month::Nov, Month::Dec, Month::Jan, Month::Feb],
        };
        // January, in time range
        assert!(w.matches(&make_now(420, DayOfWeek::Mon, Month::Jan)));
        // June, in time range
        assert!(!w.matches(&make_now(420, DayOfWeek::Mon, Month::Jun)));
    }

    #[test]
    fn test_empty_window_matches_everything() {
        let w = TimeWindow {
            after: None,
            before: None,
            days: vec![],
            months: vec![],
        };
        assert!(w.matches(&make_now(0, DayOfWeek::Mon, Month::Jan)));
        assert!(w.matches(&make_now(1439, DayOfWeek::Sun, Month::Dec)));
    }

    #[test]
    fn test_schedule_any() {
        let sched = Schedule::Any {
            any: vec![
                Schedule::Window(TimeWindow {
                    after: Some(TimeOfDay::from_hm_str("22:00").unwrap()),
                    before: Some(TimeOfDay::from_hm_str("06:00").unwrap()),
                    days: vec![
                        DayOfWeek::Mon,
                        DayOfWeek::Tue,
                        DayOfWeek::Wed,
                        DayOfWeek::Thu,
                        DayOfWeek::Fri,
                    ],
                    months: vec![],
                }),
                Schedule::Window(TimeWindow {
                    after: Some(TimeOfDay::from_hm_str("20:00").unwrap()),
                    before: Some(TimeOfDay::from_hm_str("08:00").unwrap()),
                    days: vec![DayOfWeek::Sat, DayOfWeek::Sun],
                    months: vec![],
                }),
            ],
        };
        // Friday 23:00 — matches first
        assert!(sched.matches(&make_now(23 * 60, DayOfWeek::Fri, Month::Mar)));
        // Saturday 21:00 — matches second
        assert!(sched.matches(&make_now(21 * 60, DayOfWeek::Sat, Month::Mar)));
        // Wednesday 15:00 — matches neither
        assert!(!sched.matches(&make_now(15 * 60, DayOfWeek::Wed, Month::Mar)));
    }

    #[test]
    fn test_schedule_all() {
        let sched = Schedule::All {
            all: vec![
                Schedule::Window(TimeWindow {
                    after: Some(TimeOfDay::from_hm_str("08:00").unwrap()),
                    before: Some(TimeOfDay::from_hm_str("17:00").unwrap()),
                    days: vec![],
                    months: vec![],
                }),
                Schedule::Window(TimeWindow {
                    after: None,
                    before: None,
                    days: vec![DayOfWeek::Mon, DayOfWeek::Wed, DayOfWeek::Fri],
                    months: vec![],
                }),
            ],
        };
        // Monday 10:00 — both match
        assert!(sched.matches(&make_now(600, DayOfWeek::Mon, Month::Jan)));
        // Tuesday 10:00 — time matches but day doesn't
        assert!(!sched.matches(&make_now(600, DayOfWeek::Tue, Month::Jan)));
    }

    #[test]
    fn test_schedule_not() {
        let sched = Schedule::Not {
            not: Box::new(Schedule::Window(TimeWindow {
                after: Some(TimeOfDay::from_hm_str("23:00").unwrap()),
                before: Some(TimeOfDay::from_hm_str("06:00").unwrap()),
                days: vec![],
                months: vec![],
            })),
        };
        // 12:00 — NOT in 23:00-06:00, so matches
        assert!(sched.matches(&make_now(720, DayOfWeek::Mon, Month::Jan)));
        // 02:00 — in 23:00-06:00, so NOT matches
        assert!(!sched.matches(&make_now(120, DayOfWeek::Mon, Month::Jan)));
    }

    #[test]
    fn test_schedule_serde_window() {
        let toml_str = r#"
after = "22:00"
before = "06:30"
"#;
        let sched: Schedule = toml::from_str(toml_str).unwrap();
        assert!(matches!(sched, Schedule::Window(_)));
    }

    #[test]
    fn test_schedule_serde_any() {
        let toml_str = r#"
any = [
    { after = "22:00", before = "06:00", days = ["mon", "tue"] },
    { after = "20:00", before = "08:00", days = ["sat", "sun"] },
]
"#;
        let sched: Schedule = toml::from_str(toml_str).unwrap();
        assert!(matches!(sched, Schedule::Any { .. }));
    }

    #[test]
    fn test_schedule_serde_with_months() {
        let toml_str = r#"
after = "06:00"
before = "09:00"
months = ["oct", "nov", "dec", "jan", "feb"]
"#;
        let sched: Schedule = toml::from_str(toml_str).unwrap();
        if let Schedule::Window(w) = &sched {
            assert_eq!(w.months.len(), 5);
        } else {
            panic!("Expected Window");
        }
    }

    #[test]
    fn test_only_after() {
        let w = TimeWindow {
            after: Some(TimeOfDay::from_hm_str("20:00").unwrap()),
            before: None,
            days: vec![],
            months: vec![],
        };
        assert!(w.matches(&make_now(21 * 60, DayOfWeek::Mon, Month::Jan)));
        assert!(!w.matches(&make_now(19 * 60, DayOfWeek::Mon, Month::Jan)));
    }

    #[test]
    fn test_only_before() {
        let w = TimeWindow {
            after: None,
            before: Some(TimeOfDay::from_hm_str("08:00").unwrap()),
            days: vec![],
            months: vec![],
        };
        assert!(w.matches(&make_now(7 * 60, DayOfWeek::Mon, Month::Jan)));
        assert!(!w.matches(&make_now(9 * 60, DayOfWeek::Mon, Month::Jan)));
    }

    #[test]
    fn test_validate_schedule() {
        let sched = Schedule::Window(TimeWindow {
            after: Some(TimeOfDay::from_hm_str("22:00").unwrap()),
            before: Some(TimeOfDay::from_hm_str("06:00").unwrap()),
            days: vec![],
            months: vec![],
        });
        assert!(validate_schedule(&sched, "test").is_ok());

        let empty_all = Schedule::All { all: vec![] };
        assert!(validate_schedule(&empty_all, "test").is_err());
    }
}
