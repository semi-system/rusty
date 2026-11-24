//! An iCalendar object of a calendar collection (RFC 4791 section 4.1): one `VCALENDAR` of version 2.0 whose components are of one kind (event, to-do or journal; time zones come along) and share one UID. What
//! the server keeps beside the text is the kind, the UID and the outer bounds of the object in time; a report on a time range filters on the bounds, so they are a superset: a recurring event is bounded by
//! its last occurrence when the rule has an end and is unbounded when it has none, and a time with a zone (or none) is widened by the largest distance between a zone and UTC (`time::ZONE_SLACK`).

use crate::content::{self, Component};
use crate::time::{parse_duration, parse_moment, ZONE_SLACK};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarObject {
    pub uid: String,
    /// `VEVENT`, `VTODO` or `VJOURNAL`.
    pub component: &'static str,
    pub starts_at: Option<i64>,
    /// `None`: not bounded (a rule that never ends, or a component with no start).
    pub ends_at: Option<i64>,
    pub recurring: bool,
}

const KINDS: [&str; 3] = ["VEVENT", "VTODO", "VJOURNAL"];
/// The longest UID the table keeps.
const MAX_UID_CHARS: usize = 255;

/// The bounds of one component: `(start, end)`, each `None` when the component does not say. The end of a bounded one is never before its start.
fn bounds(c: &Component) -> (Option<i64>, Option<i64>) {
    let moment = |name: &str| c.prop(name).and_then(|p| parse_moment(&p.value).map(|m| (m, p)));
    let Some((start, sp)) = moment("DTSTART").or_else(|| if c.name == "VTODO" { moment("DUE") } else { None }) else { return (None, None) };
    let slack = if start.exact || sp.param("VALUE") == Some("DATE") { 0 } else { ZONE_SLACK };
    let s = start.secs - slack;
    let mut end: Option<i64> = None;
    let end_name = if c.name == "VTODO" { "DUE" } else { "DTEND" };
    if let Some((e, ep)) = moment(end_name) {
        let eslack = if e.exact || ep.param("VALUE") == Some("DATE") { 0 } else { ZONE_SLACK };
        end = Some(e.secs + eslack);
    } else if let Some(d) = c.prop("DURATION").and_then(|p| parse_duration(&p.value)) {
        let dur = d.max(0);
        end = Some(start.secs + dur + slack);
    } else if start.date_only && c.name == "VEVENT" {
        let dur = 86_400;
        end = Some(start.secs + dur);
    }
    let end = end.unwrap_or(start.secs + slack).max(s);
    (Some(s), Some(end))
}

/// The length of one occurrence of a component, for the end of the last one of a rule.
fn span(c: &Component) -> i64 {
    let (s, e) = bounds(c);
    match (s, e) {
        (Some(s), Some(e)) => (e - s).max(0),
        _ => 0,
    }
}

/// The upper bound of the end of the last occurrence of a recurring master: `None` when the rule never ends.
fn recurrence_end(master: &Component, start: i64) -> Option<i64> {
    let span = span(master);
    let mut last: Option<i64> = None;
    let mut unbounded = false;
    for rule in master.props_named("RRULE") {
        let mut freq = None;
        let (mut count, mut until, mut interval) = (None, None, 1i64);
        for part in rule.value.split(';') {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            match k.to_ascii_uppercase().as_str() {
                "FREQ" => freq = Some(v.to_ascii_uppercase()),
                "COUNT" => count = v.parse::<i64>().ok().filter(|n| (1..=1_000_000).contains(n)),
                "UNTIL" => until = parse_moment(v),
                "INTERVAL" => interval = v.parse::<i64>().ok().filter(|n| (1..=1_000_000).contains(n)).unwrap_or(1),
                _ => {}
            }
        }
        let period = match freq.as_deref() {
            Some("SECONDLY") => 1,
            Some("MINUTELY") => 60,
            Some("HOURLY") => 3600,
            Some("DAILY") => 86_400,
            Some("WEEKLY") => 7 * 86_400,
            Some("MONTHLY") => 31 * 86_400,
            Some("YEARLY") => 366 * 86_400,
            _ => {
                unbounded = true;
                continue;
            }
        };
        let end = if let Some(u) = until {
            Some(u.secs + if u.exact { 0 } else { ZONE_SLACK } + span)
        } else {
            // The first occurrence is the start itself: COUNT occurrences are COUNT - 1 steps after it.
            count.and_then(|n| (n - 1).checked_mul(interval)?.checked_mul(period)?.checked_add(start)?.checked_add(span))
        };
        match end {
            Some(e) => last = Some(last.map_or(e, |l| l.max(e))),
            None => unbounded = true,
        }
    }
    for rdate in master.props_named("RDATE") {
        for v in rdate.value.split(',') {
            // A value is a date, a date-time or a period (`start/end`, `start/duration`): its start is what counts.
            let first = v.split('/').next().unwrap_or("");
            match parse_moment(first) {
                Some(m) => {
                    let e = m.secs + if m.exact { 0 } else { ZONE_SLACK } + span;
                    last = Some(last.map_or(e, |l| l.max(e)));
                }
                None => unbounded = true,
            }
        }
    }
    if unbounded {
        None
    } else {
        last
    }
}

/// Read the object for a calendar collection; `Err` is the reason it is not one (a fixed text).
pub fn read(text: &str) -> Result<CalendarObject, &'static str> {
    let root = content::parse(text)?;
    if root.name != "VCALENDAR" {
        return Err("the object is not a VCALENDAR");
    }
    if root.prop("VERSION").map(|p| p.value.as_str()) != Some("2.0") {
        return Err("the version is not 2.0");
    }
    let comps: Vec<&Component> = root.children.iter().filter(|c| c.name != "VTIMEZONE").collect();
    let Some(first) = comps.first() else { return Err("the calendar has no event, to-do or journal") };
    let Some(kind) = KINDS.iter().find(|k| **k == first.name) else { return Err("a component of this kind is not kept") };
    let mut uid: Option<&str> = None;
    for c in &comps {
        if c.name != *kind {
            return Err("the components are of more than one kind");
        }
        let Some(u) = c.prop("UID").map(|p| p.value.as_str()).filter(|u| !u.is_empty()) else { return Err("a component has no UID") };
        if u.chars().count() > MAX_UID_CHARS || u.chars().any(char::is_control) {
            return Err("the UID is not valid");
        }
        if uid.is_some_and(|x| x != u) {
            return Err("the components have more than one UID");
        }
        uid = Some(u);
    }
    let masters: Vec<&&Component> = comps.iter().filter(|c| c.prop("RECURRENCE-ID").is_none()).collect();
    if masters.len() > 1 {
        return Err("more than one component without a RECURRENCE-ID");
    }
    let mut start: Option<i64> = None;
    let mut end: Option<i64> = None;
    let mut known = true;
    let mut ends_known = true;
    for c in &comps {
        let (s, e) = bounds(c);
        match (s, e) {
            (Some(s), Some(mut e)) => {
                if c.prop("RECURRENCE-ID").is_none() {
                    match recurrence_end(c, s) {
                        Some(r) => e = e.max(r),
                        None if c.prop("RRULE").is_some() || c.prop("RDATE").is_some() => ends_known = false,
                        None => {}
                    }
                }
                start = Some(start.map_or(s, |x| x.min(s)));
                end = Some(end.map_or(e, |x| x.max(e)));
            }
            _ => known = false,
        }
    }
    let recurring = masters.first().is_some_and(|m| m.prop("RRULE").is_some() || m.prop("RDATE").is_some());
    let (starts_at, ends_at) = if known { (start, if ends_known { end } else { None }) } else { (None, None) };
    Ok(CalendarObject { uid: uid.unwrap_or_default().to_owned(), component: kind, starts_at, ends_at, recurring })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(body: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n{body}END:VCALENDAR\r\n")
    }

    #[test]
    fn an_event_in_utc_has_its_exact_bounds_and_one_with_a_zone_is_widened() {
        let o = read(&cal("BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\nEND:VEVENT\r\n")).unwrap();
        assert_eq!((o.uid.as_str(), o.component, o.starts_at, o.ends_at, o.recurring), ("a", "VEVENT", Some(1_704_103_200), Some(1_704_106_800), false));
        let z = read(&cal("BEGIN:VEVENT\r\nUID:a\r\nDTSTART;TZID=Europe/Berlin:20240101T100000\r\nDTEND;TZID=Europe/Berlin:20240101T110000\r\nEND:VEVENT\r\n")).unwrap();
        assert_eq!((z.starts_at, z.ends_at), (Some(1_704_103_200 - ZONE_SLACK), Some(1_704_106_800 + ZONE_SLACK)));
        let dur = read(&cal("BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T100000Z\r\nDURATION:PT30M\r\nEND:VEVENT\r\n")).unwrap();
        assert_eq!(dur.ends_at, Some(1_704_103_200 + 1800));
        let instant = read(&cal("BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T100000Z\r\nEND:VEVENT\r\n")).unwrap();
        assert_eq!((instant.starts_at, instant.ends_at), (Some(1_704_103_200), Some(1_704_103_200)));
    }

    #[test]
    fn an_all_day_event_covers_its_days_exactly() {
        let one = read(&cal("BEGIN:VEVENT\r\nUID:a\r\nDTSTART;VALUE=DATE:20240101\r\nEND:VEVENT\r\n")).unwrap();
        assert_eq!((one.starts_at, one.ends_at), (Some(1_704_067_200), Some(1_704_067_200 + 86_400)));
        let two = read(&cal("BEGIN:VEVENT\r\nUID:a\r\nDTSTART;VALUE=DATE:20240101\r\nDTEND;VALUE=DATE:20240103\r\nEND:VEVENT\r\n")).unwrap();
        assert_eq!(two.ends_at, Some(1_704_067_200 + 2 * 86_400));
    }

    #[test]
    fn a_rule_with_an_end_is_bounded_by_its_last_occurrence_and_one_without_is_not() {
        let head = "BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\n";
        let until = read(&cal(&format!("{head}RRULE:FREQ=WEEKLY;UNTIL=20240201T100000Z\r\nEND:VEVENT\r\n"))).unwrap();
        assert_eq!((until.recurring, until.ends_at), (true, Some(1_706_781_600 + 3600)));
        let count = read(&cal(&format!("{head}RRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\n"))).unwrap();
        assert_eq!(count.ends_at, Some(1_704_106_800 + 2 * 86_400), "the third day, a day is the longest it can be");
        let forever = read(&cal(&format!("{head}RRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n"))).unwrap();
        assert_eq!((forever.recurring, forever.starts_at.is_some(), forever.ends_at), (true, true, None));
        let strange = read(&cal(&format!("{head}RRULE:FREQ=SOMETIMES;COUNT=2\r\nEND:VEVENT\r\n"))).unwrap();
        assert_eq!(strange.ends_at, None, "a rule that is not understood is not bounded");
        let rdate = read(&cal(&format!("{head}RDATE:20240301T100000Z,20240201T100000Z\r\nEND:VEVENT\r\n"))).unwrap();
        assert_eq!((rdate.recurring, rdate.ends_at), (true, Some(1_709_287_200 + 3600)));
    }

    #[test]
    fn an_override_extends_the_bounds_and_the_rules_of_the_object_are_checked() {
        let both = read(&cal(
            "BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:a\r\nRECURRENCE-ID:20240102T100000Z\r\nDTSTART:20240110T100000Z\r\nDTEND:20240110T110000Z\r\nEND:VEVENT\r\n",
        ))
        .unwrap();
        assert_eq!((both.starts_at, both.ends_at), (Some(1_704_103_200), Some(1_704_880_800 + 3600)));
        let tz = "BEGIN:VTIMEZONE\r\nTZID:X\r\nEND:VTIMEZONE\r\n";
        assert!(read(&cal(&format!("{tz}BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\n"))).is_ok(), "a time zone comes along");
        for (why, body) in [
            ("no component", String::new()),
            ("no UID", "BEGIN:VEVENT\r\nDTSTART:20240101T100000Z\r\nEND:VEVENT\r\n".to_owned()),
            ("two kinds", "BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\nBEGIN:VTODO\r\nUID:a\r\nEND:VTODO\r\n".to_owned()),
            ("two UIDs", "BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:b\r\nRECURRENCE-ID:20240101T100000Z\r\nEND:VEVENT\r\n".to_owned()),
            ("two masters", "BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\n".to_owned()),
            ("a kind that is not kept", "BEGIN:VFREEBUSY\r\nUID:a\r\nEND:VFREEBUSY\r\n".to_owned()),
            ("a long UID", format!("BEGIN:VEVENT\r\nUID:{}\r\nEND:VEVENT\r\n", "u".repeat(256))),
        ] {
            assert!(read(&cal(&body)).is_err(), "{why}");
        }
        assert!(read("BEGIN:VCALENDAR\r\nVERSION:1.0\r\nBEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").is_err());
        assert!(read("BEGIN:VCARD\r\nVERSION:2.0\r\nEND:VCARD\r\n").is_err());
        // A to-do is bounded by its due time; one with nothing is not bounded.
        let todo = read(&cal("BEGIN:VTODO\r\nUID:t\r\nDUE:20240101T100000Z\r\nEND:VTODO\r\n")).unwrap();
        assert_eq!((todo.component, todo.starts_at, todo.ends_at), ("VTODO", Some(1_704_103_200), Some(1_704_103_200)));
        let bare = read(&cal("BEGIN:VTODO\r\nUID:t\r\nEND:VTODO\r\n")).unwrap();
        assert_eq!((bare.starts_at, bare.ends_at), (None, None));
    }
}
