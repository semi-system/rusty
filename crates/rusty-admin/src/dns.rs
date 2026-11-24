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

// ---- hosting: the mode of the domain and the records the tenant adds (docs/design-authoritative-dns.md, sections 3 and 4) ----

#[derive(Serialize)]
struct ExtraJson {
    id: i64,
    name: String,
    #[serde(rename = "type")]
    rtype: String,
    value: String,
    ttl: u32,
}

#[derive(Serialize)]
struct HostingJson {
    /// `external` (the domain's records are kept where it has them today) or `hosted` (the platform's name servers answer for it).
    mode: String,
    /// The names to set at the registrar when the mode is `hosted`.
    name_servers: Vec<String>,
    extra: Vec<ExtraJson>,
    max_extra: i64,
}

async fn hosting_state(app: &AdminApp, domain: rusty_core::DomainId) -> Response {
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let mode = match s.dns_zone.mode(domain).await {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let extra = match s.dns_zone.extras(domain).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let name_servers = s.mail.as_ref().map(|m| m.name_servers.clone()).unwrap_or_default();
    json(200, &HostingJson { mode, name_servers, extra: extra.into_iter().map(|e| ExtraJson { id: e.id, name: e.name, rtype: e.rtype, value: e.value, ttl: e.ttl }).collect(), max_extra: rusty_store::MAX_EXTRA_RECORDS })
}

async fn hosting_get(app: &AdminApp, req: Request) -> Response {
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
    hosting_state(app, dom.id).await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModeBody {
    mode: String,
}

/// Only a platform administrator hosts a domain: the platform's name servers then answer for it.
async fn hosting_put(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = crate::manage::platform_only(&p) {
        return r;
    }
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<ModeBody>(&req.body) else { return err(400, "invalid") };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    if s.mail.as_ref().is_none_or(|m| m.name_servers.is_empty()) && b.mode == "hosted" {
        // Hosting needs the names of the name servers to hand out; without them the domain would be delegated to nobody.
        return err(503, "unavailable");
    }
    if let Err(e) = s.dns_zone.set_mode(dom.id, &b.mode).await {
        return fail(&e);
    }
    hosting_state(app, dom.id).await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtraBody {
    name: String,
    #[serde(rename = "type")]
    rtype: String,
    value: String,
    ttl: Option<u32>,
}

async fn extra_add(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<ExtraBody>(&req.body) else { return err(400, "invalid") };
    let Ok((name, rtype, value)) = rusty_core::dns::validate_extra(&b.name, &b.rtype, &b.value) else { return err(400, "invalid") };
    let ttl = b.ttl.unwrap_or(3600);
    if !(60..=86_400).contains(&ttl) {
        return err(400, "invalid");
    }
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    // A CNAME stands alone at its name (RFC 1034): the zone refuses the others, so the tenant is told now.
    let existing = match s.dns_zone.extras(dom.id).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let same_name = existing.iter().filter(|e| e.name == name);
    if (rtype == "CNAME" && same_name.clone().next().is_some()) || same_name.clone().any(|e| e.rtype == "CNAME") {
        return err(409, "conflict");
    }
    if let Err(e) = s.dns_zone.add_extra(dom.id, &name, &rtype, &value, ttl).await {
        return fail(&e);
    }
    hosting_state(app, dom.id).await
}

async fn extra_remove(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.param("id").and_then(|i| i.parse::<i64>().ok()).filter(|n| *n > 0) else { return err(404, "not_found") };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match s.dns_zone.remove_extra(dom.id, id).await {
        Ok(true) => hosting_state(app, dom.id).await,
        Ok(false) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

macro_rules! route {
    ($router:expr, $app:expr, $method:expr, $path:expr, |$a:ident, $r:ident| $body:expr) => {{
        let $a = $app.clone();
        $router.route($method, $path, handler(move |$r: Request| {
            let $a = $a.clone();
            async move {
                // What this request changes, told to the open streams once it succeeded (docs/design-live-updates.md).
                let topics = crate::live::topics_of(&$r);
                let resp = $body;
                $a.announce_after(&resp, topics);
                resp
            }
        }))
    }};
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/dns", |a, r| dns_get(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/dns/check", |a, r| dns_check(&a, r).await);
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/dns/hosting", |a, r| hosting_get(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/dns/hosting", |a, r| hosting_put(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/dns/extra", |a, r| extra_add(&a, r).await);
    route!(router, app, Method::Delete, "/api/v1/domains/{d}/dns/extra/{id}", |a, r| extra_remove(&a, r).await)
}
