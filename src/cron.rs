//! Five-field cron expressions: minute, hour, day of month, month, day of
//! week.
//!
//! Each field is `*`, a number, a range (`1-5`), a list (`1,15`), or any of
//! those with a step (`*/15`, `8-18/2`). Day of week is 0 to 7, where both 0
//! and 7 are Sunday. As in cron, when both day fields are restricted a time
//! matches if either does. Names (`MON`, `JAN`) and `@daily`-style shortcuts
//! are not supported.

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDateTime, Timelike};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    /// Whether each day field was `*`, which decides how they combine.
    any_day: bool,
    any_weekday: bool,
}

impl Cron {
    pub fn parse(expression: &str) -> Result<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            bail!(
                "a schedule has five fields (minute hour day-of-month month day-of-week), \
not {}",
                fields.len()
            );
        };
        let mut weekdays = field(weekday, 0, 7).context("day of week")?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(Self {
            minutes: field(minute, 0, 59).context("minute")?,
            hours: field(hour, 0, 23).context("hour")?,
            days: field(day, 1, 31).context("day of month")?,
            months: field(month, 1, 12).context("month")?,
            weekdays,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    /// Whether the schedule fires in the minute `time` falls in.
    pub fn matches(&self, time: NaiveDateTime) -> bool {
        let has = |set: u64, value: u32| set & (1 << value) != 0;
        has(self.minutes, time.minute())
            && has(self.hours, time.hour())
            && self.months_has(time.month())
            && self.day_matches(time)
    }

    /// The first minute after `after` the schedule fires in, looking up to
    /// four years ahead (a 29 February schedule can be that far away).
    pub fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut time = after.with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);
        let limit = time + chrono::Duration::days(4 * 366);
        while time < limit {
            if !self.months_has(time.month()) || !self.day_matches(time) {
                // Skip the rest of a day that cannot match.
                time = (time.date() + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)?;
            } else if self.matches(time) {
                return Some(time);
            } else {
                time += chrono::Duration::minutes(1);
            }
        }
        None
    }

    /// The most times the schedule can fire in one day.
    pub fn fires_per_day(&self) -> u32 {
        self.minutes.count_ones() * self.hours.count_ones()
    }

    fn months_has(&self, month: u32) -> bool {
        self.months & (1 << month) != 0
    }

    fn day_matches(&self, time: NaiveDateTime) -> bool {
        let day = self.days & (1 << time.day()) != 0;
        let weekday = self.weekdays & (1 << time.weekday().num_days_from_sunday()) != 0;
        match (self.any_day, self.any_weekday) {
            (false, false) => day || weekday,
            _ => day && weekday,
        }
    }
}

/// One field as a bit set over `low..=high`.
fn field(text: &str, low: u32, high: u32) -> Result<u64> {
    let mut set = 0;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|step| *step > 0)
                    .with_context(|| format!("{part:?} has a bad step"))?;
                (range, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (low, high)
        } else if let Some((start, end)) = range.split_once('-') {
            (number(start, low, high)?, number(end, low, high)?)
        } else {
            let value = number(range, low, high)?;
            // `5/15` means "from 5, every 15".
            (value, if part.contains('/') { high } else { value })
        };
        if start > end {
            bail!("{part:?} runs backwards");
        }
        let mut value = start;
        while value <= end {
            set |= 1 << value;
            value += step;
        }
    }
    Ok(set)
}

fn number(text: &str, low: u32, high: u32) -> Result<u32> {
    text.parse()
        .ok()
        .filter(|value| (low..=high).contains(value))
        .with_context(|| format!("{text:?} is not a number from {low} to {high}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, 30)
            .unwrap()
    }

    #[test]
    fn fields_match_values_ranges_lists_and_steps() {
        // 23 November 2026 is a Monday.
        let monday_nine = at(2026, 11, 23, 9, 0);
        for (expression, expected) in [
            ("0 9 * * *", true),
            ("0 9 * * 1", true),
            ("0 9 * * 1-5", true),
            ("0 9 * * 0,6", false),
            ("*/15 * * * *", true),
            ("5/15 * * * *", false),
            ("0 8-18/2 * * *", false),
            ("0 9 23 11 *", true),
            ("0 9 24 * *", false),
            ("* * * 12 *", false),
        ] {
            let cron = Cron::parse(expression).unwrap();
            assert_eq!(cron.matches(monday_nine), expected, "{expression}");
        }
        assert!(
            Cron::parse("5/15 * * * *")
                .unwrap()
                .matches(at(2026, 11, 23, 9, 20))
        );
    }

    #[test]
    fn sunday_is_zero_or_seven_and_the_day_fields_combine_as_in_cron() {
        let sunday = at(2026, 11, 22, 9, 0);
        assert!(Cron::parse("0 9 * * 0").unwrap().matches(sunday));
        assert!(Cron::parse("0 9 * * 7").unwrap().matches(sunday));
        // Both day fields restricted: either may match.
        let either = Cron::parse("0 9 1 * 0").unwrap();
        assert!(either.matches(sunday));
        assert!(either.matches(at(2026, 12, 1, 9, 0)));
        assert!(!either.matches(at(2026, 11, 23, 9, 0)));
    }

    #[test]
    fn malformed_expressions_say_which_field_is_wrong() {
        for (expression, message) in [
            ("0 9 * *", "five fields"),
            ("0 9 * * * *", "five fields"),
            ("60 9 * * *", "minute"),
            ("0 24 * * *", "hour"),
            ("0 9 0 * *", "day of month"),
            ("0 9 * 13 *", "month"),
            ("0 9 * * 8", "day of week"),
            ("0 9 * * MON", "day of week"),
            ("*/0 9 * * *", "minute"),
            ("10-5 9 * * *", "minute"),
            ("@daily", "five fields"),
        ] {
            let error = Cron::parse(expression).unwrap_err();
            assert!(
                format!("{error:#}").contains(message),
                "{expression}: {error:#}"
            );
        }
    }

    #[test]
    fn next_after_finds_the_following_firing() {
        let cron = Cron::parse("30 8 * * 1").unwrap();
        // From Monday 09:00, the next Monday 08:30 is a week on.
        assert_eq!(
            cron.next_after(at(2026, 11, 23, 9, 0)),
            Some(at(2026, 11, 30, 8, 30).with_second(0).unwrap())
        );
        // Strictly after: a time inside the firing minute moves to the next.
        let hourly = Cron::parse("0 * * * *").unwrap();
        assert_eq!(
            hourly.next_after(at(2026, 11, 23, 9, 0)),
            Some(at(2026, 11, 23, 10, 0).with_second(0).unwrap())
        );
        let leap_day = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            leap_day.next_after(at(2026, 11, 23, 9, 0)),
            Some(at(2028, 2, 29, 0, 0).with_second(0).unwrap())
        );
        // 31 February never comes.
        assert_eq!(
            Cron::parse("0 0 31 2 *")
                .unwrap()
                .next_after(at(2026, 1, 1, 0, 0)),
            None
        );
    }

    #[test]
    fn fires_per_day_counts_minutes_times_hours() {
        assert_eq!(Cron::parse("0 9 * * *").unwrap().fires_per_day(), 1);
        assert_eq!(Cron::parse("*/15 * * * *").unwrap().fires_per_day(), 96);
        assert_eq!(Cron::parse("* * * * *").unwrap().fires_per_day(), 1440);
    }
}
