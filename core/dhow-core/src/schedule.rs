//! Queue schedules (ENGINE-SPEC §11): weekly windows in local time.
//!
//! Pure: callers pass "now" as a [`Zoned`], so DST and the time zone come
//! from its zone's rules. A window whose end is at or before its start runs
//! past midnight and belongs to the day it starts; start == end is all day.

use jiff::civil::{Date, Time, Weekday};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Downloads start only inside a window; running ones continue past its end.
    Start,
    /// Running downloads are paused at the window's end and resumed at the
    /// next start.
    StartAndStopOutside,
}

/// Days of the week, `mon`..`sun`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Weekdays(u8);

const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

impl Weekdays {
    pub const ALL: Self = Self(0x7f);

    pub fn contains(self, day: Weekday) -> bool {
        self.0 & (1 << (day.to_monday_zero_offset() as u8)) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `daily`, `weekdays`, `weekends`, or a comma list of `mon`..`sun`
    /// and ranges like `mon-fri`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "daily" | "all" => return Some(Self::ALL),
            "weekdays" => return Some(Self(0x1f)),
            "weekends" => return Some(Self(0x60)),
            _ => {}
        }
        let index = |d: &str| NAMES.iter().position(|n| *n == d.trim());
        let mut bits = 0u8;
        for part in s.split(',') {
            match part.split_once('-') {
                Some((a, b)) => {
                    let (a, b) = (index(a)?, index(b)?);
                    let mut i = a;
                    loop {
                        bits |= 1 << i;
                        if i == b {
                            break;
                        }
                        i = (i + 1) % 7;
                    }
                }
                None => bits |= 1 << index(part)?,
            }
        }
        (bits != 0).then_some(Self(bits))
    }

    fn names(self) -> Vec<&'static str> {
        (0..7)
            .filter(|i| self.0 & (1 << i) != 0)
            .map(|i| NAMES[i])
            .collect()
    }
}

impl Serialize for Weekdays {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.names().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Weekdays {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(d)?;
        Weekdays::parse(&names.join(","))
            .ok_or_else(|| serde::de::Error::custom("expected weekday names"))
    }
}

/// `HH:MM` or `HH:MM:SS`, local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock(Time);

impl Clock {
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split(':').map(|p| p.parse::<i8>().ok());
        let (h, m) = (parts.next()??, parts.next()??);
        let sec = match parts.next() {
            Some(p) => p?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        Time::new(h, m, sec, 0).ok().map(Self)
    }
}

impl Serialize for Clock {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let t = self.0;
        let text = if t.second() == 0 {
            format!("{:02}:{:02}", t.hour(), t.minute())
        } else {
            format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
        };
        text.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Clock {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Clock::parse(&s).ok_or_else(|| serde::de::Error::custom("expected HH:MM[:SS]"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub days: Weekdays,
    pub start: Clock,
    pub end: Clock,
}

impl Window {
    /// `<days> <start>-<end>`, e.g. `mon-fri 22:00-06:00`.
    pub fn parse(s: &str) -> Option<Self> {
        let (days, times) = s.trim().rsplit_once(' ')?;
        let (start, end) = times.split_once('-')?;
        Some(Self {
            days: Weekdays::parse(days)?,
            start: Clock::parse(start)?,
            end: Clock::parse(end)?,
        })
    }

    /// The occurrence starting on `date`, as instants in `now`'s zone.
    /// DST gaps and folds resolve the "compatible" way (a skipped time
    /// becomes the first one after the gap).
    fn on(&self, date: Date, now: &Zoned) -> Option<(Timestamp, Timestamp)> {
        if !self.days.contains(date.weekday()) {
            return None;
        }
        let tz = now.time_zone().clone();
        let start = date.to_datetime(self.start.0).to_zoned(tz.clone()).ok()?;
        let end_date = if self.end.0 <= self.start.0 {
            date.tomorrow().ok()?
        } else {
            date
        };
        let end = end_date.to_datetime(self.end.0).to_zoned(tz).ok()?;
        Some((start.timestamp(), end.timestamp()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub windows: Vec<Window>,
    pub action: Action,
}

impl Schedule {
    /// Window occurrences that could matter around `now`: from yesterday
    /// (a wrapped window still running) to eight days on (a full week).
    fn occurrences(&self, now: &Zoned) -> impl Iterator<Item = (Timestamp, Timestamp)> + '_ {
        let today = now.date();
        let first = today.yesterday().unwrap_or(today);
        let now = now.clone();
        (0..10)
            .filter_map(move |d| first.checked_add(jiff::Span::new().days(d)).ok())
            .flat_map(move |date| {
                let now = now.clone();
                self.windows.iter().filter_map(move |w| w.on(date, &now))
            })
    }

    pub fn inside(&self, now: &Zoned) -> bool {
        let t = now.timestamp();
        self.occurrences(now)
            .any(|(start, end)| start <= t && t < end)
    }

    /// The next window start or end after `now`.
    pub fn next_edge(&self, now: &Zoned) -> Option<Timestamp> {
        let t = now.timestamp();
        self.occurrences(now)
            .flat_map(|(start, end)| [start, end])
            .filter(|&e| e > t)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    fn at(tz: &TimeZone, s: &str) -> Zoned {
        s.parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(tz.clone())
            .unwrap()
    }

    fn utc(s: &str) -> Zoned {
        at(&TimeZone::UTC, s)
    }

    fn sched(windows: &[&str], action: Action) -> Schedule {
        Schedule {
            windows: windows.iter().map(|w| Window::parse(w).unwrap()).collect(),
            action,
        }
    }

    fn ts(z: &Zoned) -> Timestamp {
        z.timestamp()
    }

    #[test]
    fn weekday_parsing() {
        assert_eq!(Weekdays::parse("daily"), Some(Weekdays::ALL));
        assert_eq!(Weekdays::parse("mon-fri"), Weekdays::parse("weekdays"));
        assert_eq!(Weekdays::parse("sat,sun"), Weekdays::parse("weekends"));
        assert_eq!(
            Weekdays::parse("fri-mon").unwrap().names(),
            ["mon", "fri", "sat", "sun"]
        );
        assert_eq!(Weekdays::parse("someday"), None);
        assert_eq!(Weekdays::parse(""), None);
    }

    #[test]
    fn clock_and_window_parsing() {
        assert!(Clock::parse("22:00").is_some());
        assert!(Clock::parse("06:30:15").is_some());
        assert!(Clock::parse("24:00").is_none());
        assert!(Clock::parse("12").is_none());
        let w = Window::parse("mon-fri 22:00-06:00").unwrap();
        assert_eq!(w.days, Weekdays::parse("weekdays").unwrap());
        assert!(Window::parse("22:00-06:00").is_none());
    }

    #[test]
    fn json_round_trip() {
        let s = sched(
            &["mon,wed 22:00-06:00", "sat 10:00:30-11:00"],
            Action::StartAndStopOutside,
        );
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"windows":[{"days":["mon","wed"],"start":"22:00","end":"06:00"},{"days":["sat"],"start":"10:00:30","end":"11:00"}],"action":"start_and_stop_outside"}"#
        );
        assert_eq!(serde_json::from_str::<Schedule>(&json).unwrap(), s);
    }

    // 2026-10-26 is a Monday.

    #[test]
    fn inside_a_plain_window() {
        let s = sched(&["mon 09:00-17:00"], Action::Start);
        assert!(s.inside(&utc("2026-10-26T09:00")));
        assert!(s.inside(&utc("2026-10-26T16:59:59")));
        assert!(!s.inside(&utc("2026-10-26T17:00")), "end is exclusive");
        assert!(!s.inside(&utc("2026-10-27T10:00")), "tuesday");
    }

    #[test]
    fn a_wrapped_window_belongs_to_its_start_day() {
        let s = sched(&["mon 22:00-06:00"], Action::Start);
        assert!(s.inside(&utc("2026-10-26T23:00")));
        assert!(
            s.inside(&utc("2026-10-27T05:59")),
            "tuesday morning, monday's window"
        );
        assert!(
            !s.inside(&utc("2026-10-27T23:00")),
            "tuesday night isn't in it"
        );
        assert!(
            !s.inside(&utc("2026-10-26T05:00")),
            "monday morning belongs to sunday"
        );
    }

    #[test]
    fn equal_start_and_end_is_all_day() {
        let s = sched(&["tue 00:00-00:00"], Action::Start);
        assert!(s.inside(&utc("2026-10-27T00:00")));
        assert!(s.inside(&utc("2026-10-27T23:59")));
        assert!(!s.inside(&utc("2026-10-28T00:00")));
    }

    #[test]
    fn next_edge_finds_starts_and_ends() {
        let s = sched(&["mon,wed 22:00-06:00"], Action::StartAndStopOutside);
        assert_eq!(
            s.next_edge(&utc("2026-10-26T12:00")),
            Some(ts(&utc("2026-10-26T22:00")))
        );
        assert_eq!(
            s.next_edge(&utc("2026-10-26T23:00")),
            Some(ts(&utc("2026-10-27T06:00")))
        );
        assert_eq!(
            s.next_edge(&utc("2026-10-27T06:00")),
            Some(ts(&utc("2026-10-28T22:00")))
        );
        // A week ahead: the next monday.
        let s = sched(&["mon 08:00-09:00"], Action::Start);
        assert_eq!(
            s.next_edge(&utc("2026-10-26T09:00")),
            Some(ts(&utc("2026-11-02T08:00")))
        );
    }

    #[test]
    fn no_windows_no_edges() {
        let s = Schedule {
            windows: vec![],
            action: Action::Start,
        };
        assert_eq!(s.next_edge(&utc("2026-10-26T12:00")), None);
        assert!(!s.inside(&utc("2026-10-26T12:00")));
    }

    #[test]
    fn dst_changes_move_the_instant_not_the_local_time() {
        // US Eastern; 2026-11-01 (Sunday) 02:00 EDT → 01:00 EST.
        let tz = TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let s = sched(&["daily 09:00-10:00"], Action::Start);
        let sat = s.next_edge(&at(&tz, "2026-10-31T08:00")).unwrap();
        let sun = s.next_edge(&at(&tz, "2026-11-01T08:00")).unwrap();
        // 09:00 local both days, but 25 hours apart.
        assert_eq!(
            sun.duration_since(sat),
            jiff::SignedDuration::from_hours(25)
        );
        assert!(s.inside(&at(&tz, "2026-11-01T09:30")));
    }

    #[test]
    fn a_window_starting_in_a_dst_gap_starts_after_it() {
        // 2026-03-08 02:00 EST → 03:00 EDT: 02:30 doesn't exist.
        let tz = TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let s = sched(&["sun 02:30-04:00"], Action::Start);
        let edge = s.next_edge(&at(&tz, "2026-03-08T00:00")).unwrap();
        assert_eq!(edge, ts(&at(&tz, "2026-03-08T03:30")));
    }
}
