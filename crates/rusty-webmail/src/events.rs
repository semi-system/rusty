//! The agenda of the person (docs/design-caldav-carddav.md): `GET /api/v1/events?from=<unix>&to=<unix>` answers the events of their calendars (kept through CalDAV) that meet the window, the repeating ones
//! expanded (`rusty_dav::recur`), at most [`MAX_ITEMS`], sorted by start. Read only: the webmail's database role may read the calendars and change nothing in them. The window is at most [`MAX_DAYS`] days.
//!
//! A time is either UTC (`utc: true`: the reader shows it in its own zone) or the wall clock the event states (`utc: false`, with its zone name in `tzid` when it names one: shown as written).

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use serde::Serialize;

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

pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    router.route(Method::Get, "/api/v1/events", handler(move |req: Request| {
        let app = app.clone();
        async move { list(&app, req).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;


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
