//! The management endpoints: domains, accounts, aliases (design section 4). Every handler starts with [`AdminApp::require`] (session, administrator from
//! the database, and the three CSRF guards on a mutation) and then decides **what the administrator may touch**:
//!
//! * a platform administrator may touch every domain; a domain administrator only its own;
//! * a domain, account or alias outside the administrator's scope is `404 not_found`, **the same answer as one that does not exist**, so the API is
//!   no oracle for other tenants' names;
//! * creating and disabling domains is for platform administrators (`403 forbidden` for a domain administrator);
//! * an administrator cannot lock itself out: disabling the account it is signed in with, or the domain that account lives in, is refused.
//!
//! The JSON shapes are fixed by `contract/v1.json`; nothing secret is ever in a response (no password hash, no key).

use std::sync::Arc;

use rusty_core::{AccountId, DomainId, Error};
use rusty_directory::{parse_address, AccountInfo, AccountStatus, AliasInfo, DomainInfo, DomainStatus};
use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp};
use crate::auth::Principal;

/// Rows in one page of any list.
pub const PAGE: usize = 100;
/// The shortest password an administrator may give an account (the same as `rustyctl account add`).
pub const MIN_ACCOUNT_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 1024;
/// Addresses every domain must have (RFC 5321 section 4.5.1); the first account of a domain receives them, as with `rustyctl`.
const REQUIRED_ALIASES: [&str; 2] = ["postmaster", "abuse"];

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

// ------------------------------------------------------------------------------------------------ shapes

#[derive(Serialize)]
struct DomainJson {
    id: i64,
    name: String,
    status: &'static str,
}

fn domain_json(d: &DomainInfo) -> DomainJson {
    DomainJson { id: d.id.0, name: d.name.clone(), status: d.status.as_str() }
}

#[derive(Serialize)]
struct AccountJson {
    id: i64,
    address: String,
    status: &'static str,
    has_password: bool,
    /// When the account last signed in (seconds; at most an hour late), `null` if it never did.
    last_login_at: Option<i64>,
    /// The plan the account is on, `null` for none.
    plan: Option<String>,
    /// Bytes the mailbox holds.
    used_bytes: i64,
    /// The quota that applies in bytes (account, plan, domain, global), `null` for unlimited.
    quota_bytes: Option<i64>,
    /// When mail last arrived (seconds), `null` if none has.
    last_delivery_at: Option<i64>,
    /// A temporary password is set: the account cannot sign in until it is changed.
    must_change_password: bool,
}

fn account_json(a: &AccountInfo, domain: &str, usage: Option<&rusty_store::AccountUsage>) -> AccountJson {
    AccountJson {
        id: a.id.0,
        address: format!("{}@{domain}", a.local),
        status: a.status.as_str(),
        has_password: a.has_password,
        last_login_at: a.last_login_at,
        plan: usage.and_then(|u| u.plan.clone()),
        used_bytes: usage.map_or(0, |u| u.used_bytes),
        quota_bytes: usage.and_then(|u| u.quota_bytes),
        last_delivery_at: usage.and_then(|u| u.last_delivery_at),
        must_change_password: usage.is_some_and(|u| u.must_change_password),
    }
}

/// The accounts of one page as JSON, with their usage, plan and quota from one read.
async fn accounts_json(app: &AdminApp, rows: &[AccountInfo], domain: &str) -> Result<Vec<AccountJson>, Error> {
    let usage = match app.services.as_ref() {
        Some(s) if !rows.is_empty() => s.plans.usage_of(&rows.iter().map(|a| a.id.0).collect::<Vec<_>>()).await?,
        _ => Vec::new(),
    };
    Ok(rows.iter().map(|a| account_json(a, domain, usage.iter().find(|u| u.account == a.id.0))).collect())
}

#[derive(Serialize)]
struct AliasJson {
    id: i64,
    alias: String,
    target: String,
}

fn alias_json(a: &AliasInfo, domain: &str) -> AliasJson {
    AliasJson { id: a.id, alias: format!("{}@{domain}", a.local), target: format!("{}@{}", a.account_local, a.account_domain) }
}

#[derive(Serialize)]
struct List<T> {
    items: Vec<T>,
    /// The cursor of the next page (`after=`), or `null` at the end.
    next: Option<i64>,
    /// How many rows match the filter in all, whatever the page (the console says "12 of 340").
    total: i64,
}

fn list<T>(items: Vec<T>, last: Option<i64>, full: bool, total: i64) -> Response
where
    T: Serialize,
{
    json(200, &List { items, next: if full { last } else { None }, total })
}

/// The query of a list: `after` (the cursor), `limit` (1 to [`PAGE`], default [`PAGE`]), `q` (a substring of the name: letters, digits and `._@+-`, at most 64, lower-cased, percent-escapes
/// read) and `status` (one of the list's own statuses). Each at most once; anything else, or a value outside these rules, is `400 invalid` (and nothing is searched).
pub(crate) struct ListQuery {
    pub after: Option<i64>,
    pub limit: usize,
    pub text: Option<String>,
    pub status: Option<String>,
}

impl ListQuery {
    pub(crate) fn filter(&self) -> rusty_directory::ListFilter {
        rusty_directory::ListFilter { text: self.text.clone(), status: self.status.clone() }
    }
}

/// `%XX` escapes decoded (a `+` stays a `+`: it is a character of an address); `None` when an escape is broken or the result is not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn list_query(req: &Request, statuses: &[&str]) -> Result<ListQuery, Response> {
    let mut q = ListQuery { after: None, limit: PAGE, text: None, status: None };
    let Some(raw) = req.query.as_deref() else { return Ok(q) };
    let (mut seen_after, mut seen_limit, mut seen_q, mut seen_status) = (false, false, false, false);
    for pair in raw.split('&') {
        let Some((k, v)) = pair.split_once('=') else { return Err(err(400, "invalid")) };
        let fresh = |seen: &mut bool| !std::mem::replace(seen, true);
        match k {
            "after" if fresh(&mut seen_after) => match v.parse::<i64>() {
                Ok(n) if n >= 0 && !v.starts_with('+') => q.after = Some(n),
                _ => return Err(err(400, "invalid")),
            },
            "limit" if fresh(&mut seen_limit) => match v.parse::<usize>() {
                Ok(n) if (1..=PAGE).contains(&n) && !v.starts_with('+') => q.limit = n,
                _ => return Err(err(400, "invalid")),
            },
            "q" if fresh(&mut seen_q) => {
                let text = percent_decode(v).map(|t| t.trim().to_ascii_lowercase()).ok_or_else(|| err(400, "invalid"))?;
                if text.len() > 64 || !text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'+' | b'-')) {
                    return Err(err(400, "invalid"));
                }
                q.text = (!text.is_empty()).then_some(text);
            }
            "status" if fresh(&mut seen_status) => {
                if !v.is_empty() {
                    if !statuses.contains(&v) {
                        return Err(err(400, "invalid"));
                    }
                    q.status = Some(v.to_owned());
                }
            }
            _ => return Err(err(400, "invalid")),
        }
    }
    Ok(q)
}

// ------------------------------------------------------------------------------------------------ helpers

fn no_query(req: &Request) -> Result<(), Response> {
    if req.query.is_some() {
        return Err(err(400, "invalid"));
    }
    Ok(())
}

fn body<T: for<'de> Deserialize<'de>>(req: &Request) -> Result<T, Response> {
    serde_json::from_slice(&req.body).map_err(|_| err(400, "invalid"))
}

fn domain_of(address: &str) -> Option<&str> {
    address.rsplit_once('@').map(|(_, d)| d)
}

/// The domain `name` if it exists **and** the administrator may touch it; otherwise `404`, whichever it was.
pub(crate) async fn scoped_domain(app: &AdminApp, p: &Principal, name: &str) -> Result<DomainInfo, Response> {
    let found = match app.directory.find_domain(name).await {
        Ok(f) => f,
        // A name that is not a domain name is "no such domain".
        Err(Error::Invalid(_)) => None,
        Err(e) => return Err(fail(&e)),
    };
    match found {
        Some(d) if p.scope.allows(d.id) => Ok(d),
        _ => Err(err(404, "not_found")),
    }
}

/// The account with this address, with its domain name, if the administrator may touch its domain.
pub(crate) async fn scoped_account(app: &AdminApp, p: &Principal, address: &str) -> Result<(AccountInfo, DomainInfo), Response> {
    let Ok((_, domain)) = parse_address(address) else { return Err(err(404, "not_found")) };
    let dom = scoped_domain(app, p, &domain).await?;
    match app.directory.find_account(address).await {
        Ok(Some(a)) if a.domain == dom.id => Ok((a, dom)),
        Ok(_) => Err(err(404, "not_found")),
        Err(e) => Err(fail(&e)),
    }
}

pub(crate) fn platform_only(p: &Principal) -> Result<(), Response> {
    if p.scope.is_platform() {
        Ok(())
    } else {
        Err(err(403, "forbidden"))
    }
}

fn check_password(pw: &str) -> Result<(), Response> {
    if pw.chars().count() < MIN_ACCOUNT_PASSWORD || pw.len() > MAX_PASSWORD {
        return Err(err(400, "invalid"));
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------ domains

async fn list_domains(app: &AdminApp, req: Request) -> Response {
    let (session, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let query = match list_query(&req, &["active", "disabled"]) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let after = query.after;
    // A domain administrator sees its own domain only (named by the address it signed in with, which is in that domain).
    if !p.scope.is_platform() {
        if after.is_some() {
            return list::<DomainJson>(Vec::new(), None, false, 0);
        }
        let Some(name) = domain_of(&session.address) else { return err(404, "not_found") };
        return match scoped_domain(app, &p, name).await {
            // Its own domain, if it matches what was asked for (the same filter as everyone's).
            Ok(d) => {
                let hit = query.text.as_deref().is_none_or(|t| d.name.contains(t)) && query.status.as_deref().is_none_or(|s| d.status.as_str() == s);
                list(if hit { vec![domain_json(&d)] } else { Vec::new() }, None, false, i64::from(hit))
            }
            Err(r) => r,
        };
    }
    let filter = query.filter();
    let (rows, total) = match tokio::try_join!(app.directory.search_domains(&filter, after.map(DomainId), query.limit), app.directory.count_domains(&filter)) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let full = rows.len() == query.limit;
    let last = rows.last().map(|d| d.id.0);
    list(rows.iter().map(domain_json).collect(), last, full, total)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDomain {
    name: String,
}

async fn create_domain(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p).and(no_query(&req)) {
        return r;
    }
    let b: NewDomain = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.name.len() > 253 {
        return err(400, "invalid");
    }
    if let Err(e) = app.directory.create_domain(&b.name).await {
        return fail(&e);
    }
    match app.directory.find_domain(&b.name).await {
        Ok(Some(d)) => json(201, &domain_json(&d)),
        Ok(None) => err(503, "busy"),
        Err(e) => fail(&e),
    }
}

async fn set_domain_status(app: &AdminApp, req: Request, status: DomainStatus) -> Response {
    let (session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p).and(no_query(&req)) {
        return r;
    }
    let name = req.param("d").unwrap_or_default().to_owned();
    let dom = match scoped_domain(app, &p, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Disabling the domain this administrator's own account lives in would lock it out of the web.
    if status == DomainStatus::Disabled && domain_of(&session.address) == Some(dom.name.as_str()) {
        return err(400, "invalid");
    }
    if let Err(e) = app.directory.set_domain_status(dom.id, status).await {
        return fail(&e);
    }
    json(200, &domain_json(&DomainInfo { status, ..dom }))
}

// ------------------------------------------------------------------------------------------------ accounts

async fn list_accounts(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let query = match list_query(&req, &["active", "locked", "disabled"]) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let filter = query.filter();
    let (rows, total) = match tokio::try_join!(app.directory.search_accounts(dom.id, &filter, query.after.map(AccountId), query.limit), app.directory.count_accounts(dom.id, &filter)) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let full = rows.len() == query.limit;
    let last = rows.last().map(|a| a.id.0);
    match accounts_json(app, &rows, &dom.name).await {
        Ok(items) => list(items, last, full, total),
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAccount {
    local: String,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Serialize)]
struct Created {
    #[serde(flatten)]
    account: AccountJson,
    /// The role addresses the first account of a domain received.
    aliases_created: Vec<String>,
}

async fn create_account(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let b: NewAccount = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.local.len() > 64 || b.local.contains('@') {
        return err(400, "invalid");
    }
    // The rules of the domain: its password rules, its limit on accounts, its default plan (docs/design-domain-settings.md).
    let settings = match crate::settings::of(app, dom.id).await {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    if let Some(pw) = &b.password {
        if settings.password_problem(pw).is_some() {
            return err(400, "invalid");
        }
    }
    if let Some(max) = settings.max_accounts {
        match app.directory.count_accounts(dom.id, &rusty_directory::ListFilter::default()).await {
            Ok(n) if u64::try_from(n).unwrap_or(u64::MAX) >= max => return err(409, "limit_reached"),
            Ok(_) => {}
            Err(e) => return fail(&e),
        }
    }
    let first = match app.directory.list_accounts(dom.id, None, 1).await {
        Ok(rows) => rows.is_empty(),
        Err(e) => return fail(&e),
    };
    let id = match app.directory.create_account(&b.local, dom.id, b.password.as_deref(), None).await {
        Ok(id) => id,
        Err(e) => return fail(&e),
    };
    if let (Some(plan), Some(s)) = (settings.default_plan.as_deref(), app.services.as_ref()) {
        if let Err(e) = s.plans.set_account_plan(id, Some(plan)).await {
            return fail(&e);
        }
    }
    let address = format!("{}@{}", b.local, dom.name);
    let info = match app.directory.find_account(&address).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(503, "busy"),
        Err(e) => return fail(&e),
    };
    let mut aliased = Vec::new();
    if first {
        for name in REQUIRED_ALIASES {
            if name == info.local {
                continue;
            }
            match app.directory.add_alias(name, dom.id, id).await {
                Ok(()) => aliased.push(format!("{name}@{}", dom.name)),
                Err(Error::Conflict(_)) => {}
                Err(e) => return fail(&e),
            }
        }
    }
    let account = match accounts_json(app, std::slice::from_ref(&info), &dom.name).await {
        Ok(mut v) => v.remove(0),
        Err(e) => return fail(&e),
    };
    json(201, &Created { account, aliases_created: aliased })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPassword {
    password: String,
    /// The password is temporary: the account cannot sign in until it is changed.
    #[serde(default)]
    must_change: bool,
}

async fn set_password(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let b: NewPassword = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if let Err(r) = check_password(&b.password) {
        return r;
    }
    // The password rules of the account's domain.
    match crate::settings::of(app, acc.domain).await {
        Ok(s) if s.password_problem(&b.password).is_some() => return err(400, "invalid"),
        Ok(_) => {}
        Err(e) => return fail(&e),
    }
    if let Err(e) = app.directory.set_password(acc.id, &b.password).await {
        return fail(&e);
    }
    // A temporary password (`must_change`) holds the account until it is changed; any other password clears that.
    if let Some(s) = app.services.as_ref() {
        if let Err(e) = s.plans.set_must_change_password(acc.id.0, b.must_change).await {
            return fail(&e);
        }
    }
    // Every session of this account ends (it may be an administrator).
    if app.auth.revoke_account(acc.id).await.is_err() {
        return err(503, "busy");
    }
    json(200, &serde_json::json!({}))
}

async fn set_account_status(app: &AdminApp, req: Request, status: AccountStatus) -> Response {
    let (session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let (acc, dom) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if status != AccountStatus::Active && acc.id == session.account_id() {
        // An administrator does not disable the account it is signed in with.
        return err(400, "invalid");
    }
    if let Err(e) = app.directory.set_status(acc.id, status).await {
        return fail(&e);
    }
    if status != AccountStatus::Active && app.auth.revoke_account(acc.id).await.is_err() {
        return err(503, "busy");
    }
    match accounts_json(app, &[AccountInfo { status, ..acc }], &dom.name).await {
        Ok(mut v) => json(200, &v.remove(0)),
        Err(e) => fail(&e),
    }
}

// ------------------------------------------------------------------------------------------------ aliases

async fn list_aliases(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let query = match list_query(&req, &[]) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let filter = query.filter();
    let (rows, total) = match tokio::try_join!(app.directory.search_aliases(dom.id, &filter, query.after, query.limit), app.directory.count_aliases(dom.id, &filter)) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let full = rows.len() == query.limit;
    let last = rows.last().map(|a| a.id);
    list(rows.iter().map(|a| alias_json(a, &dom.name)).collect(), last, full, total)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAlias {
    local: String,
    /// The full address of the account the alias points at (it must be in the same domain).
    target: String,
}

async fn create_alias(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let b: NewAlias = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.local.len() > 64 || b.local.contains('@') || b.target.len() > 320 {
        return err(400, "invalid");
    }
    // The target is looked up in the administrator's scope: an account of another tenant is "no such account", never a different answer.
    let (target, _) = match scoped_account(app, &p, &b.target).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if target.domain != dom.id {
        return err(400, "invalid");
    }
    if let Err(e) = app.directory.add_alias(&b.local, dom.id, target.id).await {
        return fail(&e);
    }
    json(201, &serde_json::json!({ "alias": format!("{}@{}", b.local.to_ascii_lowercase(), dom.name), "target": b.target.to_ascii_lowercase() }))
}

async fn remove_alias(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let address = req.param("a").unwrap_or_default().to_owned();
    let Ok((_, domain)) = parse_address(&address) else { return err(404, "not_found") };
    if let Err(r) = scoped_domain(app, &p, &domain).await {
        return r;
    }
    match app.directory.remove_alias(&address).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(e) => fail(&e),
    }
}

/// Delete a CLOSED account now: the garbage collector removes it and its mailbox at its next pass (a closed account is removed by itself after `closed_account_days`). An account that is not
/// closed is `409 not_closed`; an administrator cannot delete the account it is signed in with.
async fn delete_account(app: &AdminApp, req: Request) -> Response {
    let (session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let (acc, dom) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if format!("{}@{}", acc.local, dom.name) == session.address {
        return err(400, "invalid");
    }
    match app.directory.request_account_delete(acc.id).await {
        Ok(true) => json(200, &serde_json::json!({ "deletion": "scheduled" })),
        Ok(false) => err(409, "not_closed"),
        Err(e) => fail(&e),
    }
}

/// Delete a domain that holds nothing (platform administrators): `409 not_empty` otherwise.
async fn delete_domain(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p).and(no_query(&req)) {
        return r;
    }
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match app.directory.delete_domain(dom.id).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(Error::Conflict(_)) => err(409, "not_empty"),
        Err(e) => fail(&e),
    }
}

// ------------------------------------------------------------------------------------------------ routes

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
    let router = route!(router, app, Method::Get, "/api/v1/domains", |a, r| list_domains(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains", |a, r| create_domain(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/disable", |a, r| set_domain_status(&a, r, DomainStatus::Disabled).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/enable", |a, r| set_domain_status(&a, r, DomainStatus::Active).await);
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/accounts", |a, r| list_accounts(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/accounts", |a, r| create_account(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/accounts/{a}/password", |a, r| set_password(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/accounts/{a}/disable", |a, r| set_account_status(&a, r, AccountStatus::Disabled).await);
    let router = route!(router, app, Method::Post, "/api/v1/accounts/{a}/lock", |a, r| set_account_status(&a, r, AccountStatus::Locked).await);
    let router = route!(router, app, Method::Post, "/api/v1/accounts/{a}/enable", |a, r| set_account_status(&a, r, AccountStatus::Active).await);
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/aliases", |a, r| list_aliases(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/aliases", |a, r| create_alias(&a, r).await);
    let router = route!(router, app, Method::Delete, "/api/v1/accounts/{a}", |a, r| delete_account(&a, r).await);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}", |a, r| delete_domain(&a, r).await);
    route!(router, app, Method::Delete, "/api/v1/aliases/{a}", |a, r| remove_alias(&a, r).await)
}
