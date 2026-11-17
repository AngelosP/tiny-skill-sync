//! Poll intervals such as `15m`, `6h`, `1d`, `2w` and `1mo`, and the date math to schedule them.
//!
//! All times are UTC seconds since the Unix epoch. A month is a calendar month: adding one month to
//! January 31 gives the last day of February.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub type UnixSecs = i64;

pub const MIN_POLL_MINUTES: u32 = 5;
const MAX_AMOUNT: u32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub amount: u32,
    pub unit: Unit,
}

impl Interval {
    pub const fn new(amount: u32, unit: Unit) -> Self {
        Interval { amount, unit }
    }

    pub fn parse(text: &str) -> Result<Interval, String> {
        let t = text.trim().to_ascii_lowercase();
        let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
        if digits_end == 0 {
            return Err(format!("'{}' must start with a number, for example 6h or 1d", text.trim()));
        }
        let amount: u32 = t[..digits_end].parse().map_err(|_| format!("'{}' has a number that is too large", text.trim()))?;
        let unit = match t[digits_end..].trim() {
            "m" | "min" | "mins" | "minute" | "minutes" => Unit::Minute,
            "h" | "hr" | "hrs" | "hour" | "hours" => Unit::Hour,
            "d" | "day" | "days" => Unit::Day,
            "w" | "wk" | "wks" | "week" | "weeks" => Unit::Week,
            "mo" | "mon" | "month" | "months" => Unit::Month,
            "" => return Err(format!("'{}' needs a unit: m, h, d, w or mo", text.trim())),
            other => return Err(format!("unknown unit '{other}' (use m, h, d, w or mo)")),
        };
        if amount == 0 {
            return Err("the interval must be more than zero".to_string());
        }
        if amount > MAX_AMOUNT {
            return Err(format!("the interval is too long (at most {MAX_AMOUNT})"));
        }
        Ok(Interval { amount, unit })
    }

    /// A poll interval must not be shorter than five minutes.
    pub fn validate_poll(self) -> Result<Interval, String> {
        if self.approx_minutes() < MIN_POLL_MINUTES as u64 {
            return Err(format!("polling more often than every {MIN_POLL_MINUTES} minutes is not allowed"));
        }
        Ok(self)
    }

    fn approx_minutes(self) -> u64 {
        let n = self.amount as u64;
        match self.unit {
            Unit::Minute => n,
            Unit::Hour => n * 60,
            Unit::Day => n * 60 * 24,
            Unit::Week => n * 60 * 24 * 7,
            Unit::Month => n * 60 * 24 * 28,
        }
    }

    /// The moment one interval after `from`.
    pub fn add_to(self, from: UnixSecs) -> UnixSecs {
        let n = self.amount as i64;
        match self.unit {
            Unit::Minute => from + n * 60,
            Unit::Hour => from + n * 3600,
            Unit::Day => from + n * 86_400,
            Unit::Week => from + n * 604_800,
            Unit::Month => add_months(from, n),
        }
    }

    /// Plain English for the window, for example "6 hours".
    pub fn describe(self) -> String {
        let word = match self.unit {
            Unit::Minute => "minute",
            Unit::Hour => "hour",
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
        };
        if self.amount == 1 { format!("1 {word}") } else { format!("{} {word}s", self.amount) }
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = match self.unit {
            Unit::Minute => "m",
            Unit::Hour => "h",
            Unit::Day => "d",
            Unit::Week => "w",
            Unit::Month => "mo",
        };
        write!(f, "{}{}", self.amount, unit)
    }
}

impl Serialize for Interval {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Interval::parse(&text).map_err(serde::de::Error::custom)
    }
}

pub fn now_secs() -> UnixSecs {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

// Civil date conversions after Howard Hinnant's public-domain algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (m as u64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

pub fn add_months(secs: UnixSecs, months: i64) -> UnixSecs {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let total = y * 12 + (m as i64 - 1) + months;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    days_from_civil(ny, nm, d.min(days_in_month(ny, nm))) * 86_400 + time_of_day
}

/// `2026-11-15T10:30:00Z`, for logs and the window.
pub fn format_utc(secs: UnixSecs) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, (t / 60) % 60, t % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: u32, h: i64, min: i64) -> UnixSecs {
        days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60
    }

    #[test]
    fn parses_every_unit_and_spelling() {
        assert_eq!(Interval::parse("15m").unwrap(), Interval::new(15, Unit::Minute));
        assert_eq!(Interval::parse("6h").unwrap(), Interval::new(6, Unit::Hour));
        assert_eq!(Interval::parse(" 1 D ").unwrap(), Interval::new(1, Unit::Day));
        assert_eq!(Interval::parse("2weeks").unwrap(), Interval::new(2, Unit::Week));
        assert_eq!(Interval::parse("1mo").unwrap(), Interval::new(1, Unit::Month));
        assert_eq!(Interval::parse("3 Months").unwrap(), Interval::new(3, Unit::Month));
    }

    #[test]
    fn rejects_bad_text() {
        for bad in ["", "d", "5", "0d", "-1d", "1.5h", "5x", "99999d", "1 d 2 h"] {
            assert!(Interval::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn text_round_trips() {
        for text in ["5m", "6h", "1d", "2w", "1mo"] {
            assert_eq!(Interval::parse(text).unwrap().to_string(), text);
        }
        let json = serde_json::to_string(&Interval::new(2, Unit::Week)).unwrap();
        assert_eq!(json, "\"2w\"");
        assert_eq!(serde_json::from_str::<Interval>("\"1mo\"").unwrap(), Interval::new(1, Unit::Month));
        assert!(serde_json::from_str::<Interval>("\"soon\"").is_err());
    }

    #[test]
    fn polls_cannot_be_faster_than_five_minutes() {
        assert!(Interval::new(4, Unit::Minute).validate_poll().is_err());
        assert!(Interval::new(5, Unit::Minute).validate_poll().is_ok());
        assert!(Interval::new(1, Unit::Hour).validate_poll().is_ok());
    }

    #[test]
    fn fixed_units_add_exact_seconds() {
        let t = at(2026, 11, 15, 10, 30);
        assert_eq!(Interval::new(90, Unit::Minute).add_to(t), t + 5400);
        assert_eq!(Interval::new(2, Unit::Hour).add_to(t), t + 7200);
        assert_eq!(Interval::new(1, Unit::Day).add_to(t), t + 86_400);
        assert_eq!(Interval::new(1, Unit::Week).add_to(t), t + 604_800);
    }

    #[test]
    fn months_follow_the_calendar() {
        let one = Interval::new(1, Unit::Month);
        assert_eq!(one.add_to(at(2026, 11, 15, 10, 30)), at(2026, 12, 15, 10, 30));
        assert_eq!(one.add_to(at(2026, 12, 31, 23, 59)), at(2027, 1, 31, 23, 59));
        assert_eq!(one.add_to(at(2027, 1, 31, 8, 0)), at(2027, 2, 28, 8, 0));
        assert_eq!(one.add_to(at(2028, 1, 31, 8, 0)), at(2028, 2, 29, 8, 0));
        assert_eq!(one.add_to(at(2026, 3, 31, 0, 0)), at(2026, 4, 30, 0, 0));
        assert_eq!(Interval::new(12, Unit::Month).add_to(at(2028, 2, 29, 5, 5)), at(2029, 2, 28, 5, 5));
    }

    #[test]
    fn civil_conversion_round_trips() {
        for days in [-800_000i64, -1, 0, 1, 59, 60, 365, 11_017, 20_000, 60_000, 800_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(format_utc(at(2026, 11, 15, 10, 30)), "2026-11-15T10:30:00Z");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
    }
}
