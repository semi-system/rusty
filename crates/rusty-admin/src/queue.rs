//! The outbound queue of a domain and the message trace (docs/design-queue.md): what is waiting to be sent for this tenant and why, the operator actions on it (hold, release now, retry what was
//! given up, delete), and the delivery log of what already happened. Domain administrators see their own domain; the platform's administrators see every domain.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{LogRow, QueueJob, HELD_UNTIL};
use serde::Serialize;

use crate::api::{fail, AdminApp};
use crate::manage::scoped_domain;

const PAGE: usize = 50;
const STATES: [&str; 7] = ["queued", "running", "failed", "dead", "done", "held", "waiting"];

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct JobJson {
    id: i64,
    /// `waiting` (queued, due), `held`, `running`, `failed`, `dead` or `done`.
    state: &'static str,
    sender: String,
    recipients: Vec<String>,
    destination: String,
    attempts: i64,
    created_at: i64,
    /// When the next attempt is due (seconds); `null` for a held job.
    next_attempt_at: Option<i64>,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct JobsJson {
    items: Vec<JobJson>,
    /// The `before=` of the next page, or `null` at the end.
    next: Option<i64>,
}

fn job_json(j: &QueueJob) -> JobJson {
    let v: serde_json::Value = serde_json::from_str(&j.payload).unwrap_or(serde_json::Value::Null);
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_owned();
    let held = j.state == "queued" && j.run_after >= HELD_UNTIL;
    let state = match j.state.as_str() {
        "queued" if held => "held",
        "queued" => "waiting",
        "running" => "running",
        "failed" => "failed",
        "dead" => "dead",
        _ => "done",
    };
    JobJson {
        id: j.id,
        state,
        sender: text("envelope_from"),
        recipients: v.get("recipients").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        destination: text("destination"),
        attempts: j.attempts,
        created_at: j.created_at,
        next_attempt_at: (j.state == "queued" && !held).then_some(j.run_after),
        last_error: j.last_error.clone(),
    }
}

#[derive(Serialize)]
struct LogJson {
    id: i64,
    /// `out` (sent) or `in` (received).
    direction: String,
    at: i64,
    sender: String,
    recipient: String,
    event: String,
    remote: Option<String>,
    status: Option<String>,
    detail: Option<String>,
}

#[derive(Serialize)]
struct TraceJson {
    items: Vec<LogJson>,
    next: Option<i64>,
}

fn log_json(r: &LogRow) -> LogJson {
    LogJson { id: r.id, direction: r.direction.clone(), at: r.at, sender: r.sender.clone(), recipient: r.recipient.clone(), event: r.event.clone(), remote: r.remote.clone(), status: r.status.clone(), detail: r.detail.clone() }
}

/// `state`, `q` and `before` of the query string; anything else, or a value that is not one of the known ones, is `400 invalid`.
/// The state filter, the text to search for (lower case) and the job or log id to page from.
type Filters = (Option<String>, Option<String>, Option<i64>);

fn params(req: &Request) -> Result<Filters, Response> {
    let (mut state, mut q, mut before) = (None, None, None);
    for part in req.query.as_deref().unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').ok_or_else(|| err(400, "invalid"))?;
        let v = percent_decode(v).ok_or_else(|| err(400, "invalid"))?;
        match k {
            "state" if STATES.contains(&v.as_str()) => state = Some(v),
            "q" if !v.is_empty() && v.len() <= 200 => q = Some(v.to_lowercase()),
            "before" => before = Some(v.parse::<i64>().map_err(|_| err(400, "invalid"))?),
            _ => return Err(err(400, "invalid")),
        }
    }
    Ok((state, q, before))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b.get(i)? {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(*c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

macro_rules! begin {
    ($app:expr, $req:expr, $mutating:expr) => {{
        let (_, p) = match $app.require(&$req, $mutating).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        let Some(s) = $app.services.as_ref() else { return err(503, "unavailable") };
        let dom = match scoped_domain($app, &p, $req.param("d").unwrap_or_default()).await {
            Ok(d) => d,
            Err(r) => return r,
        };
        (s, dom)
    }};
}

async fn list(app: &AdminApp, req: Request) -> Response {
    let (s, dom) = begin!(app, req, false);
    let (state, q, before) = match params(&req) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // `held` and `waiting` are views of the queued jobs; the database knows them as such.
    let rows = match s.queue.jobs(dom.id, state.as_deref(), q.as_deref(), before, PAGE as i64).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let next = (rows.len() == PAGE).then(|| rows.last().map(|j| j.id)).flatten();
    json(200, &JobsJson { items: rows.iter().map(job_json).collect(), next })
}

async fn act(app: &AdminApp, req: Request, action: &'static str) -> Response {
    let (s, dom) = begin!(app, req, true);
    let Ok(id) = req.param("n").unwrap_or_default().parse::<i64>() else { return err(404, "not_found") };
    let done = match action {
        "hold" => s.queue.hold(dom.id, id).await,
        "release" => s.queue.release(dom.id, id).await,
        "retry" => s.queue.requeue(dom.id, id).await,
        _ => s.queue.delete(dom.id, id).await,
    };
    match done {
        Ok(true) => json(200, &serde_json::json!({})),
        // No such job of this domain, or it is in a state the action does not apply to (a worker holds it, or it was already settled).
        Ok(false) => err(409, "wrong_state"),
        Err(e) => fail(&e),
    }
}

async fn trace(app: &AdminApp, req: Request) -> Response {
    let (s, dom) = begin!(app, req, false);
    let (_, q, before) = match params(&req) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let rows = match s.queue.trace(dom.id, q.as_deref(), before, PAGE as i64).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let next = (rows.len() == PAGE).then(|| rows.last().map(|r| r.id)).flatten();
    json(200, &TraceJson { items: rows.iter().map(log_json).collect(), next })
}

/// One day of the statistics page. `received`: mail accepted for a mailbox of the domain; `rejected`: mail refused after DATA; `sent`: delivered to the remote server; `deferred`: attempts that
/// will be tried again; `failed`: bounced or given up.
#[derive(Serialize, Default)]
struct DayJson {
    /// The start of the UTC day, in seconds.
    day: i64,
    received: i64,
    rejected: i64,
    sent: i64,
    deferred: i64,
    failed: i64,
}

#[derive(Serialize)]
struct StatsJson {
    days: Vec<DayJson>,
}

/// `GET /api/v1/domains/{d}/stats`: the counts of the last 30 days (the log is kept that long), newest day first.
async fn stats(app: &AdminApp, req: Request) -> Response {
    let (s, dom) = begin!(app, req, false);
    let now = match s.jobs.now().await {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    let rows = match s.queue.stats(dom.id, now.saturating_sub(30 * 86_400)).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let mut days: Vec<DayJson> = Vec::new();
    for r in rows {
        let start = r.day.saturating_mul(86_400);
        if days.last().is_none_or(|d| d.day != start) {
            days.push(DayJson { day: start, ..DayJson::default() });
        }
        let Some(d) = days.last_mut() else { continue };
        match (r.direction.as_str(), r.event.as_str()) {
            ("in", "delivered") => d.received += r.count,
            ("in", _) => d.rejected += r.count,
            (_, "delivered") => d.sent += r.count,
            (_, "deferred") => d.deferred += r.count,
            _ => d.failed += r.count,
        }
    }
    days.reverse();
    json(200, &StatsJson { days })
}

macro_rules! route {
    ($router:expr, $app:expr, $method:expr, $path:expr, |$a:ident, $r:ident| $body:expr) => {{
        let $a = $app.clone();
        $router.route($method, $path, handler(move |$r: Request| {
            let $a = $a.clone();
            async move { $body }
        }))
    }};
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/queue", |a, r| list(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/queue/{n}/hold", |a, r| act(&a, r, "hold").await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/queue/{n}/release", |a, r| act(&a, r, "release").await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/queue/{n}/retry", |a, r| act(&a, r, "retry").await);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}/queue/{n}", |a, r| act(&a, r, "delete").await);
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/stats", |a, r| stats(&a, r).await);
    route!(router, app, Method::Get, "/api/v1/domains/{d}/trace", |a, r| trace(&a, r).await)
}
