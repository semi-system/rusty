//! The agenda of the person (docs/design-caldav-carddav.md): `GET /api/v1/events?from=<unix>&to=<unix>` answers the events of their calendars (kept through CalDAV) that meet the window, the repeating ones
//! expanded (`rusty_dav::recur`), at most [`MAX_ITEMS`], sorted by start. Read only: the webmail's database role may read the calendars and change nothing in them. The window is at most [`MAX_DAYS`] days.
//!
//! A time is either UTC (`utc: true`: the reader shows it in its own zone) or the wall clock the event states (`utc: false`, with its zone name in `tzid` when it names one: shown as written).

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::{error, WebmailApp};

/// The longest window one request may ask for.
const MAX_DAYS: i64 = 62;
const MAX_ITEMS: usize = 500;
/// How many calendar objects are read per calendar.
const OBJECTS_PER_CALENDAR: i64 = 2000;
/// A time zone is at most this far from UTC: the bounds the store keeps are widened by it for an event that gives no zone, so the range read widens its window the same way.
const SLACK: i64 = 14 * 3600;

#[derive(Serialize)]
struct Item {
    uid: String,
    summary: String,
    location: String,
    start: i64,
    end: i64,
    all_day: bool,
    utc: bool,
    tzid: Option<String>,
    /// It repeats by a rule that is not expanded: only its first occurrence is listed.
    unexpanded: bool,
    calendar: String,
}

#[derive(Serialize)]
struct Items {
    items: Vec<Item>,
    /// There were more than the list holds.
    truncated: bool,
}

/// `from` and `to` of the query: two whole numbers, `from < to`, at most [`MAX_DAYS`] apart (and nothing else in the query).
fn window(query: Option<&str>) -> Option<(i64, i64)> {
    let (mut from, mut to) = (None, None);
    for p in query?.split('&') {
        match p.split_once('=')? {
            ("from", v) if from.is_none() => from = Some(v.parse::<i64>().ok()?),
            ("to", v) if to.is_none() => to = Some(v.parse::<i64>().ok()?),
            _ => return None,
        }
    }
    let (from, to) = (from?, to?);
    (from >= 0 && to > from && to - from <= MAX_DAYS * 86_400 && to < 253_402_300_800).then_some((from, to))
}

async fn list(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some((from, to)) = window(req.query.as_deref()) else { return error(400, "invalid") };
    let account = session.account_id();
    let Ok(calendars) = app.dav.collections(account, "calendar").await else { return error(503, "busy") };
    let mut items: Vec<Item> = Vec::new();
    for cal in calendars {
        let Ok(objects) = app.dav.objects_in_range(cal.id, Some("VEVENT"), Some(from - SLACK), Some(to + SLACK), OBJECTS_PER_CALENDAR).await else { return error(503, "busy") };
        for o in objects {
            for x in rusty_dav::recur::occurrences(&o.body, from, to, MAX_ITEMS + 1) {
                items.push(Item { uid: x.uid, summary: x.summary, location: x.location, start: x.start, end: x.end, all_day: x.all_day, utc: x.utc, tzid: x.tzid, unexpanded: x.unexpanded, calendar: cal.display_name.clone() });
            }
        }
    }
    items.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.cmp(&b.uid)));
    let truncated = items.len() > MAX_ITEMS;
    items.truncate(MAX_ITEMS);
    Response::json(200, &Items { items, truncated }).with_header("cache-control", "no-store")
}

const MAX_SUMMARY: usize = 200;
const MAX_LOCATION: usize = 200;
/// The longest event the form makes.
const MAX_SPAN_DAYS: i64 = 366;

#[derive(Deserialize)]
struct NewEvent {
    summary: String,
    #[serde(default)]
    location: String,
    /// Seconds since the epoch: the start in UTC, or for a whole-day event the UTC midnight of its first day.
    start: i64,
    /// The end in UTC; for a whole-day event the UTC midnight after its last day.
    end: i64,
    #[serde(default)]
    all_day: bool,
}

#[derive(Serialize)]
struct Made {
    uid: String,
}

/// A text value of iCalendar (RFC 5545 section 3.3.11): backslash, semicolon and comma escaped, a line end written as `\n`.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// The UID and the stored form of a new event, or why it is refused. `uid_random` makes the UID; `now` is the stamp.
fn build(e: &NewEvent, uid_random: [u8; 16], now: i64) -> Result<(String, rusty_store::NewDavObject), &'static str> {
    let summary = e.summary.trim();
    let location = e.location.trim();
    if summary.is_empty() || summary.chars().count() > MAX_SUMMARY || location.chars().count() > MAX_LOCATION {
        return Err("text");
    }
    if [summary, location].iter().any(|t| t.chars().any(char::is_control)) {
        return Err("text");
    }
    let day = 86_400;
    let ok_time = e.start >= 0 && e.end > e.start && e.end - e.start <= MAX_SPAN_DAYS * day && e.end < 253_402_300_800;
    if !ok_time || (e.all_day && (e.start % day != 0 || e.end % day != 0)) {
        return Err("time");
    }
    let uid = format!("urn:uuid:{}", hex::encode(uid_random));
    let (s, t) = (rusty_dav::recur::format_moment(e.start, e.all_day), rusty_dav::recur::format_moment(e.end, e.all_day));
    let value = if e.all_day { ";VALUE=DATE" } else { "" };
    let loc = if location.is_empty() { String::new() } else { format!("LOCATION:{}\r\n", escape_text(location)) };
    let body = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Rusty//Webmail//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{}\r\nDTSTART{value}:{s}\r\nDTEND{value}:{t}\r\nSUMMARY:{}\r\n{loc}END:VEVENT\r\nEND:VCALENDAR\r\n",
        rusty_dav::recur::format_moment(now, false),
        escape_text(summary)
    );
    let read = rusty_dav::ical::read(&body)?;
    let name = format!("{}.ics", hex::encode(&Sha256::digest(uid.as_bytes())[..16]));
    Ok((uid.clone(), rusty_store::NewDavObject { name, uid, body, component: read.component.into(), starts_at: read.starts_at, ends_at: read.ends_at, recurring: read.recurring }))
}

/// `POST /api/v1/events` (session and CSRF token): a single event in the default calendar of the person (or the first one), written through `DavRepo::put` as the DAV service writes, so a CalDAV client sees it
/// at once. A person with no calendar gets a 409: the DAV service makes the calendar, never this.
async fn create(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(e) = serde_json::from_slice::<NewEvent>(&req.body) else { return error(400, "invalid") };
    let mut random = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut random);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let Ok((uid, object)) = build(&e, random, now) else { return error(400, "invalid") };
    let Ok(calendars) = app.dav.collections(session.account_id(), "calendar").await else { return error(503, "busy") };
    let Some(cal) = calendars.iter().find(|c| c.name == "default").or(calendars.first()) else { return error(409, "no_calendar") };
    match app.dav.put(cal.id, &object, &rusty_store::DavPrecondition::Absent).await {
        Ok(rusty_store::DavPutResult::Created { .. }) => Response::json(201, &Made { uid }).with_header("cache-control", "no-store"),
        Ok(rusty_store::DavPutResult::Full(_)) => error(409, "full"),
        Ok(_) => error(409, "conflict"),
        Err(_) => error(503, "busy"),
    }
}

pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    let creator = app.clone();
    router
        .route(Method::Post, "/api/v1/events", handler(move |req: Request| {
            let app = creator.clone();
            async move { create(&app, req).await }
        }))
        .route(Method::Get, "/api/v1/events", handler(move |req: Request| {
            let app = app.clone();
            async move { list(&app, req).await }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn a_new_event_is_written_as_a_calendar_object_the_server_reads_back() {
        let day = 86_400;
        let e = NewEvent { summary: " Lunch, with; Ada ".into(), location: "Cafe\\1".into(), start: 1_709_634_000, end: 1_709_637_600, all_day: false };
        let (uid, o) = build(&e, [9; 16], 1_709_600_000).unwrap();
        assert!(uid.starts_with("urn:uuid:0909") && o.uid == uid && o.name.ends_with(".ics") && rusty_dav::paths::name_ok(&o.name));
        assert!(o.body.contains("SUMMARY:Lunch\\, with\\; Ada\r\n") && o.body.contains("LOCATION:Cafe\\\\1\r\n") && o.body.contains("DTSTART:20240305T102000Z\r\n"), "{}", o.body);
        assert_eq!((o.component.as_str(), o.starts_at.is_some(), o.recurring), ("VEVENT", true, false));
        let occ = rusty_dav::recur::occurrences(&o.body, 0, 4_000_000_000, 10);
        assert_eq!((occ[0].summary.as_str(), occ[0].location.as_str(), occ[0].start, occ[0].end), ("Lunch, with; Ada", "Cafe\\1", 1_709_634_000, 1_709_637_600));
        // A whole-day event is dates.
        let d = NewEvent { summary: "Holiday".into(), location: String::new(), start: 19_787 * day, end: 19_789 * day, all_day: true };
        let (_, o) = build(&d, [1; 16], 0).unwrap();
        assert!(o.body.contains("DTSTART;VALUE=DATE:20240305\r\n") && o.body.contains("DTEND;VALUE=DATE:20240307\r\n") && !o.body.contains("LOCATION"), "{}", o.body);
        // Refused: no title, control characters, an end not after the start, too long, a whole-day event not on midnights, a negative time.
        let bad = |s: &str, st: i64, en: i64, all: bool| build(&NewEvent { summary: s.into(), location: String::new(), start: st, end: en, all_day: all }, [0; 16], 0).is_err();
        assert!(bad("", 0, 10, false) && bad("a\u{0}b", 0, 10, false) && bad("x", 10, 10, false) && bad("x", 10, 5, false) && bad("x", 0, 400 * day, false) && bad("x", 5, day + 5, true) && bad("x", -1, 5, false));
        assert!(build(&NewEvent { summary: "x".repeat(201), location: String::new(), start: 0, end: 5, all_day: false }, [0; 16], 0).is_err());
    }

    #[test]
    fn the_window_is_two_numbers_in_order_and_bounded() {
        assert_eq!(window(Some("from=100&to=200")), Some((100, 200)));
        assert_eq!(window(Some("to=200&from=100")), Some((100, 200)));
        let day = 86_400;
        assert!(window(Some(&format!("from=0&to={}", 62 * day))).is_some());
        for bad in ["", "from=1", "to=2", "from=2&to=1", "from=1&to=1", "from=a&to=2", "from=1&to=2&x=3", "from=1&from=2&to=9", "from=-5&to=9", &format!("from=0&to={}", 63 * day), "from=1&to=99999999999999"] {
            assert_eq!(window(Some(bad)), None, "{bad}");
        }
    }
}
