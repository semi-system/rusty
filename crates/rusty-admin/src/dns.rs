//! The DNS page of a domain (design section 7): the records to publish and what the last check found.
//!
//! * `GET /api/v1/domains/{d}/dns` generates the records from the operator's facts (`[admin] public_ips`, `spf_include`, the host name) and the domain's stored
//!   DKIM keys with the same code as `rustyctl domain dns`, and joins them with the stored drift state. A record whose expected value changed since its last
//!   check (a new key, a new address) shows as `unchecked`: the stored status described another record.
//! * `POST /api/v1/domains/{d}/dns/check` queues a `dns.check` job. The admin does not resolve names; the service that can query DNS runs the check and records the
//!   result, which the next `GET` shows.
//!
//! Scope rules are those of [`crate::manage`].

use std::sync::Arc;

use rusty_core::dns::{records_with_facts, MailFacts, Requirement};
use rusty_core::dns_check::record_hash;
use rusty_http::{handler, Method, Request, Response, Router};
use serde::Serialize;

use crate::api::{fail, AdminApp};
use crate::manage::scoped_domain;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct RecordJson {
    name: String,
    #[serde(rename = "type")]
    rtype: &'static str,
    value: String,
    ttl: u32,
    purpose: &'static str,
    /// `required`, `recommended` or `optional`.
    required: &'static str,
    /// `ok`, `missing`, `mismatch`, `pending`, `unknown` ... as the check recorded it, or `unchecked`.
    status: String,
    /// What DNS showed when it was not `ok`.
    seen: Option<String>,
    last_checked_at: Option<i64>,
}

#[derive(Serialize)]
struct RecordsJson {
    records: Vec<RecordJson>,
    warnings: Vec<&'static str>,
}

fn requirement(r: Requirement) -> &'static str {
    match r {
        Requirement::Required => "required",
        Requirement::Recommended => "recommended",
        Requirement::Optional => "optional",
    }
}

async fn dns_get(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if req.query.is_some() {
        return err(400, "invalid");
    }
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let Some(mail) = s.mail.as_ref() else { return err(503, "unavailable") };
    let stored = match s.dns.dkim_keys(dom.id).await {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    let keys: Vec<(&str, &str, &str)> = stored.iter().map(|k| (k.selector.as_str(), k.algorithm.as_str(), k.public_key_b64.as_str())).collect();
    let facts = MailFacts { mail_host: &mail.host, public_ips: &mail.public_ips, spf_include: mail.spf_include.as_deref() };
    let (records, warnings) = match records_with_facts(&dom.name, &facts, &keys) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let states = match s.dns.states(dom.id).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let items = records
        .iter()
        .map(|r| {
            let hash = record_hash(r);
            let name = r.name.to_ascii_lowercase();
            let state = states.iter().find(|st| st.key.name == name && st.key.rtype == r.rtype.as_str() && st.key.purpose == r.purpose && st.expected_hash == hash);
            RecordJson {
                name: r.name.clone(),
                rtype: r.rtype.as_str(),
                value: r.value.clone(),
                ttl: r.ttl_hint,
                purpose: r.purpose,
                required: requirement(r.required),
                status: state.map_or_else(|| "unchecked".to_owned(), |st| st.status.as_str().to_owned()),
                seen: state.and_then(|st| st.seen.clone()),
                last_checked_at: state.map(|st| st.last_checked_at),
            }
        })
        .collect();
    json(200, &RecordsJson { records: items, warnings })
}

async fn dns_check(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if req.query.is_some() {
        return err(400, "invalid");
    }
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    if let Err(e) = rusty_smtp::outbound::dns_job::enqueue_dns_check(&s.jobs, dom.id, &dom.name).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({ "queued": true }))
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
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/dns", |a, r| dns_get(&a, r).await);
    route!(router, app, Method::Post, "/api/v1/domains/{d}/dns/check", |a, r| dns_check(&a, r).await)
}
