//! The occurrences of a calendar event inside a window of time, for a view that lists them (the agenda of the webmail): the recurrence rule of RFC 5545 section 3.3.10 for the common rules (daily, weekly,
//! monthly and yearly, with INTERVAL, COUNT, UNTIL, BYDAY, BYMONTHDAY and BYMONTH), EXDATE, RDATE and the overrides of single occurrences (RECURRENCE-ID).
//!
//! There is no time zone database in this crate (`time.rs`), so every time is kept as the **wall clock** of the event, in seconds as if it were UTC: a rule is defined on the wall clock of the zone of the
//! event, so expanding it there is right whatever the zone does with daylight saving time. An occurrence says whether its time is UTC (`utc`: the reader converts it to its own zone) or a wall clock
//! (floating, or a `TZID`, named in `tzid`: shown as written). The window is widened by [`ZONE_SLACK`] for the times that are not UTC, so an occurrence near its edge is never missed (the caller drops what
//! the exact zone puts outside).
//!
//! A rule outside the supported set (BYSETPOS, BYWEEKNO, BYYEARDAY, BYHOUR and the like, or BYDAY with a number in a weekly or yearly rule) is not guessed at: the event is listed once, at its start, with
//! `unexpanded` set, so a view can say that it repeats.

use std::collections::HashSet;

use crate::content::{self, Component};
use crate::time::{days_from_civil, parse_duration, parse_moment, Moment, ZONE_SLACK};

/// How many periods of a rule are walked at most (a rule that never matches ends here).
const MAX_PERIODS: i64 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub uid: String,
    pub summary: String,
    pub location: String,
    /// Seconds since the epoch of the start; the wall clock as if UTC unless `utc`.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub utc: bool,
    /// The zone name the event states (`Europe/Berlin`), if it states one.
    pub tzid: Option<String>,
    /// The event repeats by a rule this module does not expand: this is its first occurrence only.
    pub unexpanded: bool,
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}


/// A moment (seconds since the epoch) as iCalendar writes it: `YYYYMMDDTHHMMSSZ` in UTC, or `YYYYMMDD` for a whole day.
pub fn format_moment(secs: i64, date_only: bool) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    if date_only {
        return format!("{y:04}{m:02}{d:02}");
    }
    let t = secs.rem_euclid(86_400);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", t / 3600, t % 3600 / 60, t % 60)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Monday is 0.
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

#[derive(Debug, Default)]
struct Rule {
    freq: String,
    interval: i64,
    count: Option<i64>,
    until: Option<i64>,
    /// `(ordinal, weekday)`, Monday 0; the ordinal is 0 when there is none.
    by_day: Vec<(i64, i64)>,
    by_month_day: Vec<i64>,
    by_month: Vec<i64>,
}

fn weekday_of(s: &str) -> Option<i64> {
    ["MO", "TU", "WE", "TH", "FR", "SA", "SU"].iter().position(|d| *d == s).map(|p| p as i64)
}

/// The rule, or `None` when it uses something that is not expanded here (or is malformed).
fn parse_rule(value: &str) -> Option<Rule> {
    let mut r = Rule { interval: 1, ..Rule::default() };
    for part in value.split(';') {
        let (k, v) = part.split_once('=')?;
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => r.freq = v.to_ascii_uppercase(),
            "INTERVAL" => r.interval = v.parse().ok().filter(|n| (1..=1000).contains(n))?,
            "COUNT" => r.count = Some(v.parse().ok().filter(|n| *n >= 1)?),
            "UNTIL" => r.until = Some(parse_moment(v)?.secs),
            "WKST" => {}
            "BYDAY" => {
                for d in v.split(',') {
                    let d = d.to_ascii_uppercase();
                    let cut = d.len().checked_sub(2)?;
                    let (num, name) = (d.get(..cut)?, d.get(cut..)?);
                    let n = if num.is_empty() { 0 } else { num.parse::<i64>().ok().filter(|n| *n != 0 && n.abs() <= 5)? };
                    r.by_day.push((n, weekday_of(name)?));
                }
            }
            "BYMONTHDAY" => {
                for d in v.split(',') {
                    r.by_month_day.push(d.parse::<i64>().ok().filter(|n| *n != 0 && n.abs() <= 31)?);
                }
            }
            "BYMONTH" => {
                for d in v.split(',') {
                    r.by_month.push(d.parse::<i64>().ok().filter(|n| (1..=12).contains(n))?);
                }
            }
            _ => return None,
        }
    }
    if !matches!(r.freq.as_str(), "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY") {
        return None;
    }
    // A number before a weekday means "the n-th in the month (or year)": only the monthly rule is expanded that way.
    if r.by_day.iter().any(|(n, _)| *n != 0) && r.freq != "MONTHLY" {
        return None;
    }
    // A day of the month in a weekly rule is not a thing; a yearly rule by weekday needs week numbers.
    if (r.freq == "WEEKLY" && !r.by_month_day.is_empty()) || (r.freq == "YEARLY" && !r.by_day.is_empty()) {
        return None;
    }
    Some(r)
}

/// The days (counted from the epoch) of the month `y`-`m` that the rule picks, in order.
fn month_days(r: &Rule, y: i64, m: i64, start_day_of_month: i64) -> Vec<i64> {
    let dim = days_in_month(y, m);
    let mut out: Vec<i64> = Vec::new();
    if !r.by_day.is_empty() {
        for (n, wd) in &r.by_day {
            let all: Vec<i64> = (1..=dim).filter(|d| days_from_civil(y, m, *d).is_some_and(|x| weekday(x) == *wd)).collect();
            let pick: Vec<i64> = match *n {
                0 => all,
                n if n > 0 => all.get((n - 1) as usize).copied().into_iter().collect(),
                n => all.len().checked_sub(n.unsigned_abs() as usize).and_then(|i| all.get(i)).copied().into_iter().collect(),
            };
            out.extend(pick);
        }
        if !r.by_month_day.is_empty() {
            let wanted: Vec<i64> = r.by_month_day.iter().map(|d| if *d > 0 { *d } else { dim + 1 + d }).collect();
            out.retain(|d| wanted.contains(d));
        }
    } else if !r.by_month_day.is_empty() {
        out = r.by_month_day.iter().map(|d| if *d > 0 { *d } else { dim + 1 + d }).filter(|d| (1..=dim).contains(d)).collect();
    } else if start_day_of_month <= dim {
        out.push(start_day_of_month);
    }
    out.sort_unstable();
    out.dedup();
    out.into_iter().filter_map(|d| days_from_civil(y, m, d)).collect()
}

/// The start of the instances of the rule (wall seconds), in order, from `s0` (the first instance, which the rule need not match: it counts anyway), while `keep_going(start)` holds.
fn instances(r: &Rule, s0: i64, mut keep_going: impl FnMut(i64) -> bool) -> Vec<i64> {
    let tod = s0.rem_euclid(86_400);
    let d0 = s0.div_euclid(86_400);
    let (y0, m0, dom0) = civil_from_days(d0);
    let mut out: Vec<i64> = vec![s0];
    let mut count = 1i64;
    let done = |count: i64| r.count.is_some_and(|c| count >= c);
    if done(count) {
        return out;
    }
    let month_ok = |day: i64| r.by_month.is_empty() || r.by_month.contains(&civil_from_days(day).1);
    for k in 0..MAX_PERIODS {
        let days: Vec<i64> = match r.freq.as_str() {
            "DAILY" => {
                let day = d0 + k * r.interval;
                let wd_ok = r.by_day.is_empty() || r.by_day.iter().any(|(_, w)| *w == weekday(day));
                let md_ok = r.by_month_day.is_empty() || r.by_month_day.iter().any(|d| {
                    let (y, m, dd) = civil_from_days(day);
                    *d == dd || (*d < 0 && days_in_month(y, m) + 1 + d == dd)
                });
                if wd_ok && md_ok && month_ok(day) { vec![day] } else { vec![] }
            }
            "WEEKLY" => {
                let week = d0 - weekday(d0) + k * 7 * r.interval;
                let wds: Vec<i64> = if r.by_day.is_empty() { vec![weekday(d0)] } else { let mut w: Vec<i64> = r.by_day.iter().map(|(_, w)| *w).collect(); w.sort_unstable(); w.dedup(); w };
                wds.into_iter().map(|w| week + w).filter(|d| month_ok(*d)).collect()
            }
            "MONTHLY" => {
                let idx = y0 * 12 + (m0 - 1) + k * r.interval;
                let (y, m) = (idx.div_euclid(12), idx.rem_euclid(12) + 1);
                if !r.by_month.is_empty() && !r.by_month.contains(&m) { vec![] } else { month_days(r, y, m, dom0) }
            }
            _ => {
                let y = y0 + k * r.interval;
                let months: Vec<i64> = if r.by_month.is_empty() { vec![m0] } else { r.by_month.clone() };
                let mut v = Vec::new();
                for m in months {
                    let dim = days_in_month(y, m);
                    let picks: Vec<i64> = if r.by_month_day.is_empty() { vec![dom0] } else { r.by_month_day.iter().map(|d| if *d > 0 { *d } else { dim + 1 + d }).collect() };
                    v.extend(picks.into_iter().filter(|d| (1..=dim).contains(d)).filter_map(|d| days_from_civil(y, m, d)));
                }
                v.sort_unstable();
                v
            }
        };
        for day in days {
            let start = day * 86_400 + tod;
            if start <= s0 {
                continue;
            }
            if r.until.is_some_and(|u| start > u) || !keep_going(start) {
                return out;
            }
            out.push(start);
            count += 1;
            if done(count) {
                return out;
            }
        }
        // The next period starts after the last one this period could hold: stop once it is past the window.
        if !keep_going(s0 + k * 86_400 * if r.freq == "YEARLY" { 365 } else if r.freq == "MONTHLY" { 28 } else if r.freq == "WEEKLY" { 7 } else { 1 } * r.interval) {
            return out;
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

struct Event<'a> {
    c: &'a Component,
    start: Moment,
    all_day: bool,
    utc: bool,
    tzid: Option<String>,
}

fn event(c: &Component) -> Option<Event<'_>> {
    let p = c.prop("DTSTART")?;
    let start = parse_moment(&p.value)?;
    Some(Event { c, start, all_day: start.date_only, utc: start.exact && !start.date_only, tzid: p.param("TZID").map(str::to_owned) })
}

fn length(e: &Event<'_>) -> i64 {
    let c = e.c;
    if let Some(end) = c.prop("DTEND").and_then(|p| parse_moment(&p.value)) {
        return (end.secs - e.start.secs).max(0);
    }
    if let Some(d) = c.prop("DURATION").and_then(|p| parse_duration(&p.value)) {
        return d.max(0);
    }
    if e.all_day { 86_400 } else { 0 }
}

fn occurrence(e: &Event<'_>, start: i64, len: i64, unexpanded: bool) -> Occurrence {
    let text = |n: &str| e.c.prop(n).map(|p| unescape(&p.value)).unwrap_or_default();
    Occurrence { uid: text("UID"), summary: text("SUMMARY"), location: text("LOCATION"), start, end: start + len, all_day: e.all_day, utc: e.utc, tzid: e.tzid.clone(), unexpanded }
}

/// The occurrences of the events of one calendar object (its text) that meet `[from, to)` (seconds since the epoch, UTC), sorted by start, at most `limit`. A text that is not a calendar has none.
pub fn occurrences(text: &str, from: i64, to: i64, limit: usize) -> Vec<Occurrence> {
    let Ok(root) = content::parse(text) else { return Vec::new() };
    let events: Vec<Event<'_>> = root.children.iter().filter(|c| c.name == "VEVENT").filter_map(event).collect();
    let (masters, overrides): (Vec<&Event<'_>>, Vec<&Event<'_>>) = events.iter().partition(|e| e.c.prop("RECURRENCE-ID").is_none());
    let mut out: Vec<Occurrence> = Vec::new();
    for m in masters {
        let len = length(m);
        let slack = if m.utc || m.all_day { 0 } else { ZONE_SLACK };
        let (lo, hi) = (from - slack, to + slack);
        let meets = |start: i64, len: i64| start < hi && (start + len > lo || (len == 0 && start >= lo));
        let rrule = m.c.prop("RRULE").map(|p| parse_rule(&p.value));
        let mut starts: Vec<i64> = match &rrule {
            None => vec![m.start.secs],
            Some(None) => {
                if meets(m.start.secs, len) {
                    out.push(occurrence(m, m.start.secs, len, true));
                }
                continue;
            }
            Some(Some(r)) => instances(r, m.start.secs, |s| s < hi),
        };
        for p in m.c.props_named("RDATE") {
            starts.extend(p.value.split(',').filter_map(parse_moment).map(|x| x.secs));
        }
        let excluded: HashSet<i64> = m.c.props_named("EXDATE").flat_map(|p| p.value.split(',').filter_map(parse_moment).map(|x| x.secs).collect::<Vec<_>>()).collect();
        starts.sort_unstable();
        starts.dedup();
        for s in starts {
            if excluded.contains(&s) {
                continue;
            }
            // An override of this very occurrence replaces it.
            let uid = m.c.prop("UID").map(|p| p.value.as_str());
            let replaced = overrides.iter().any(|o| o.c.prop("UID").map(|p| p.value.as_str()) == uid && o.c.prop("RECURRENCE-ID").and_then(|p| parse_moment(&p.value)).is_some_and(|r| r.secs == s));
            if !replaced && meets(s, len) {
                out.push(occurrence(m, s, len, false));
            }
        }
    }
    for o in overrides {
        let len = length(o);
        let slack = if o.utc || o.all_day { 0 } else { ZONE_SLACK };
        if o.start.secs < to + slack && (o.start.secs + len > from - slack || (len == 0 && o.start.secs >= from - slack)) {
            out.push(occurrence(o, o.start.secs, len, false));
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.cmp(&b.uid)));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(body: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//y//EN\r\n{body}END:VCALENDAR\r\n")
    }

    fn ev(extra: &str) -> String {
        cal(&format!("BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Stand\\, up\r\nDTSTAMP:20240101T000000Z\r\n{extra}END:VEVENT\r\n"))
    }

    fn day(y: i64, m: i64, d: i64) -> i64 {
        days_from_civil(y, m, d).unwrap() * 86_400
    }

    fn starts(text: &str, from: i64, to: i64) -> Vec<(i64, i64, i64)> {
        occurrences(text, from, to, 1000).into_iter().map(|o| civil_from_days(o.start.div_euclid(86_400))).collect()
    }

    #[test]
    fn civil_dates_round_trip_and_weekdays_are_right() {
        for d in [-1000, 0, 59, 11_016, 19_723, 25_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), Some(d));
        }
        assert_eq!(weekday(days_from_civil(2024, 1, 1).unwrap()), 0, "2024-01-01 is a Monday");
        assert_eq!(weekday(days_from_civil(1970, 1, 1).unwrap()), 3, "1970-01-01 is a Thursday");
    }

    #[test]
    fn a_single_event_is_listed_with_its_text_and_only_when_it_meets_the_window() {
        let t = ev("DTSTART:20240305T100000Z\r\nDTEND:20240305T113000Z\r\nLOCATION:Room 1\\nFloor 2\r\n");
        let o = occurrences(&t, day(2024, 3, 1), day(2024, 3, 31), 10);
        assert_eq!(o.len(), 1);
        assert_eq!((o[0].summary.as_str(), o[0].location.as_str(), o[0].utc, o[0].unexpanded), ("Stand, up", "Room 1\nFloor 2", true, false));
        assert_eq!(o[0].end - o[0].start, 5400);
        assert!(occurrences(&t, day(2024, 4, 1), day(2024, 4, 30), 10).is_empty());
        assert!(occurrences(&t, day(2024, 3, 6), day(2024, 3, 7), 10).is_empty(), "an event that ended before the window is not in it");
    }

    #[test]
    fn a_zone_is_kept_as_written_and_a_whole_day_event_lasts_a_day() {
        let t = ev("DTSTART;TZID=Europe/Berlin:20240305T090000\r\nDTEND;TZID=Europe/Berlin:20240305T100000\r\n");
        let o = occurrences(&t, day(2024, 3, 5), day(2024, 3, 6), 10);
        assert_eq!((o[0].utc, o[0].tzid.as_deref(), o[0].start % 86_400), (false, Some("Europe/Berlin"), 9 * 3600));
        let d = ev("DTSTART;VALUE=DATE:20240305\r\n");
        let o = occurrences(&d, day(2024, 3, 5), day(2024, 3, 6), 10);
        assert_eq!((o[0].all_day, o[0].end - o[0].start), (true, 86_400));
    }

    #[test]
    fn daily_weekly_monthly_and_yearly_rules_expand_with_interval_count_and_until() {
        let daily = ev("DTSTART:20240301T080000Z\r\nDTEND:20240301T083000Z\r\nRRULE:FREQ=DAILY;INTERVAL=2;COUNT=4\r\n");
        assert_eq!(starts(&daily, 0, day(2025, 1, 1)), [(2024, 3, 1), (2024, 3, 3), (2024, 3, 5), (2024, 3, 7)]);
        // Every Monday and Thursday of every second week (weeks start on Monday), until a date; the first instance counts though it is a Friday.
        let weekly = ev("DTSTART:20240301T080000Z\r\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,TH;UNTIL=20240331T000000Z\r\n");
        assert_eq!(starts(&weekly, 0, day(2025, 1, 1)), [(2024, 3, 1), (2024, 3, 11), (2024, 3, 14), (2024, 3, 25), (2024, 3, 28)]);
        let monthly = ev("DTSTART:20240131T080000Z\r\nRRULE:FREQ=MONTHLY;COUNT=3\r\n");
        assert_eq!(starts(&monthly, 0, day(2025, 1, 1)), [(2024, 1, 31), (2024, 3, 31), (2024, 5, 31)], "a month without the 31st is skipped");
        let yearly = ev("DTSTART:20240229T080000Z\r\nRRULE:FREQ=YEARLY;COUNT=2\r\n");
        assert_eq!(starts(&yearly, 0, day(2040, 1, 1)), [(2024, 2, 29), (2028, 2, 29)], "a year without the 29th of February is skipped");
    }

    #[test]
    fn monthly_rules_by_weekday_and_by_day_of_the_month() {
        let second_tuesday = ev("DTSTART:20240109T080000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=2TU;COUNT=3\r\n");
        assert_eq!(starts(&second_tuesday, 0, day(2025, 1, 1)), [(2024, 1, 9), (2024, 2, 13), (2024, 3, 12)]);
        let last_friday = ev("DTSTART:20240126T080000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=-1FR;COUNT=3\r\n");
        assert_eq!(starts(&last_friday, 0, day(2025, 1, 1)), [(2024, 1, 26), (2024, 2, 23), (2024, 3, 29)]);
        let last_day = ev("DTSTART:20240131T080000Z\r\nRRULE:FREQ=MONTHLY;BYMONTHDAY=-1;COUNT=3\r\n");
        assert_eq!(starts(&last_day, 0, day(2025, 1, 1)), [(2024, 1, 31), (2024, 2, 29), (2024, 3, 31)]);
    }

    #[test]
    fn a_rule_that_never_ends_is_cut_by_the_window_and_the_limit() {
        let t = ev("DTSTART:20240101T080000Z\r\nRRULE:FREQ=DAILY\r\n");
        assert_eq!(occurrences(&t, day(2024, 6, 1), day(2024, 6, 8), 100).len(), 7);
        assert_eq!(occurrences(&t, day(2024, 6, 1), day(2030, 1, 1), 5).len(), 5);
    }

    #[test]
    fn excluded_added_and_overridden_occurrences() {
        let t = cal(
            "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Weekly\r\nDTSTART:20240301T080000Z\r\nDTEND:20240301T090000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEXDATE:20240308T080000Z\r\nRDATE:20240303T080000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Moved\r\nRECURRENCE-ID:20240315T080000Z\r\nDTSTART:20240316T120000Z\r\nDTEND:20240316T130000Z\r\nEND:VEVENT\r\n",
        );
        let o = occurrences(&t, 0, day(2025, 1, 1), 100);
        let got: Vec<(String, (i64, i64, i64))> = o.iter().map(|x| (x.summary.clone(), civil_from_days(x.start.div_euclid(86_400)))).collect();
        assert_eq!(
            got,
            [("Weekly".into(), (2024, 3, 1)), ("Weekly".into(), (2024, 3, 3)), ("Moved".into(), (2024, 3, 16)), ("Weekly".into(), (2024, 3, 22))],
            "the 8th is excluded, the 3rd is added, the 15th moved to the 16th"
        );
    }

    #[test]
    fn a_rule_that_is_not_expanded_lists_the_event_once_and_says_so() {
        let t = ev("DTSTART:20240301T080000Z\r\nRRULE:FREQ=MONTHLY;BYSETPOS=1;BYDAY=MO,TU\r\n");
        let o = occurrences(&t, day(2024, 3, 1), day(2024, 3, 2), 10);
        assert_eq!((o.len(), o[0].unexpanded), (1, true));
        // A nonsense rule is the same, and a text that is no calendar has nothing.
        assert!(occurrences(&ev("DTSTART:20240301T080000Z\r\nRRULE:FREQ=SOMETIMES\r\n"), 0, day(2025, 1, 1), 10)[0].unexpanded);
        assert!(occurrences("not a calendar", 0, 10, 10).is_empty());
    }


    #[test]
    fn a_moment_is_written_the_way_it_is_read() {
        let secs = day(2024, 3, 5) + 9 * 3600 + 7 * 60 + 3;
        assert_eq!(format_moment(secs, false), "20240305T090703Z");
        assert_eq!(parse_moment(&format_moment(secs, false)).unwrap().secs, secs);
        assert_eq!(format_moment(secs, true), "20240305");
        assert_eq!(parse_moment("20240305").unwrap().secs, day(2024, 3, 5));
    }

    #[test]
    fn hostile_rules_never_panic_and_end() {
        for rule in [
            "FREQ=DAILY;BYDAY=é", "FREQ=DAILY;BYDAY=ééé", "FREQ=YEARLY;INTERVAL=1000;COUNT=99999999999", "FREQ=MONTHLY;BYMONTHDAY=31;BYMONTH=2", "FREQ=WEEKLY;BYDAY=", "FREQ=;", "=;=", "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;UNTIL=00000101T000000Z", "FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30", "FREQ=MONTHLY;BYDAY=5MO;COUNT=3", "FREQ=DAILY;COUNT=0", "FREQ=WEEKLY;BYDAY=99MO",
        ] {
            let t = ev(&format!("DTSTART:99991231T235959Z\r\nRRULE:{rule}\r\n"));
            let _ = occurrences(&t, 0, i64::MAX / 4, 50);
            let t = ev(&format!("DTSTART:20240101T000000Z\r\nRRULE:{rule}\r\n"));
            let _ = occurrences(&t, 0, day(2100, 1, 1), 50);
        }
    }

    #[test]
    fn a_window_edge_does_not_lose_an_event_of_a_zone() {
        // 23:30 wall clock on the 5th in a zone ahead of UTC is still the 5th UTC-side near the edge of a window ending at the 6th 00:00 UTC.
        let t = ev("DTSTART;TZID=Asia/Tokyo:20240306T023000\r\nDTEND;TZID=Asia/Tokyo:20240306T033000\r\n");
        assert_eq!(occurrences(&t, day(2024, 3, 5), day(2024, 3, 6), 10).len(), 1, "the wall clock says the 6th; the caller sorts out the zone");
    }
}
