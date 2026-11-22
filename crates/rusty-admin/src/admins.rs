//! Who may use the admin web (docs/design-admin-parity.md, item 1): list, grant, revoke.
//!
//! * A platform administrator sees every administrator and may grant and revoke; a domain administrator sees the administrators of its own domain
//!   and nothing else (a grant of another domain is not in its list, and it cannot touch grants: `403 forbidden`).
//! * A grant needs an account that can sign in (it has a password); one that is already an administrator is `409 exists` (a change of scope is a
//!   revoke and a new grant, so that no step can lower the last platform administrator by the side door).
//! * Nobody revokes their own grant, and the last platform administrator cannot be revoked (`400 invalid`, `409 exists`): the console is never left
//!   with no one who can sign in. The check and the delete are one step in the store.

use std::sync::Arc;

use rusty_directory::parse_address;
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{AdminEntry, AdminGrant, Revoked};
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp};
use crate::manage::{list_query, platform_only, scoped_domain};

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct AdminJson {
    /// The account id (the cursor of the list).
    id: i64,
    address: String,
    /// `platform` or `domain`.
    scope: &'static str,
    /// `admin`, or `helpdesk` (a domain administrator that only resets passwords and locks accounts).
    role: &'static str,
    domain: Option<String>,
    created_at: i64,
}

fn admin_json(e: &AdminEntry) -> AdminJson {
    let domain = e.grant.domain.and_then(|_| e.address.rsplit_once('@').map(|(_, d)| d.to_owned()));
    AdminJson { id: e.grant.account.0, address: e.address.clone(), scope: if e.grant.domain.is_some() { "domain" } else { "platform" }, role: if e.grant.helpdesk { "helpdesk" } else { "admin" }, domain, created_at: e.grant.created_at }
}

#[derive(Serialize)]
struct AdminList {
    items: Vec<AdminJson>,
    next: Option<i64>,
    /// How many administrators match in all, whatever the page.
    total: i64,
}

async fn list_admins(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let query = match list_query(&req, &[]) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let only = match p.scope {
        crate::auth::Scope::Platform => None,
        crate::auth::Scope::Domain(d) => Some(d),
    };
    let after = query.after.map(rusty_core::AccountId);
    let text = query.text.as_deref();
    let (rows, total) = match tokio::try_join!(app.admins.list_named(after, query.limit as i64, only, text), app.admins.count_named(only, text)) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let next = if rows.len() == query.limit { rows.last().map(|e| e.grant.account.0) } else { None };
    Response::json(200, &AdminList { items: rows.iter().map(admin_json).collect(), next, total }).with_header("cache-control", "no-store")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    address: String,
    /// `null`: every domain (a platform administrator); a name: the administrator of that domain only.
    domain: Option<String>,
    /// `admin` (the default) or `helpdesk`; helpdesk needs a domain.
    #[serde(default)]
    role: Option<String>,
}

async fn grant_admin(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p) {
        return r;
    }
    if req.query.is_some() {
        return err(400, "invalid");
    }
    let b: Grant = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => return err(400, "invalid"),
    };
    if b.address.len() > 320 || parse_address(&b.address).is_err() {
        return err(400, "invalid");
    }
    let account = match app.directory.find_account(&b.address).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    if !account.has_password {
        return err(400, "invalid");
    }
    let helpdesk = match b.role.as_deref() {
        None | Some("admin") => false,
        Some("helpdesk") => true,
        Some(_) => return err(400, "invalid"),
    };
    if helpdesk && b.domain.is_none() {
        return err(400, "invalid");
    }
    let domain = match b.domain.as_deref() {
        None => None,
        Some(name) => match scoped_domain(app, &p, name).await {
            Ok(d) => Some(d),
            Err(r) => return r,
        },
    };
    match app.admins.get(account.id).await {
        Ok(None) => {}
        Ok(Some(_)) => return err(409, "exists"),
        Err(e) => return fail(&e),
    }
    // A domain administrator must be an account of that domain (the table refuses anything else; the answer is `400 invalid`).
    let g = match app.admins.grant(account.id, domain.as_ref().map(|d| d.id)).await {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };
    let g = if helpdesk {
        match app.admins.set_helpdesk(account.id, true).await {
            Ok(_) => AdminGrant { helpdesk: true, ..g },
            Err(e) => return fail(&e),
        }
    } else {
        g
    };
    let entry = AdminEntry { grant: g, address: b.address.trim().to_ascii_lowercase() };
    Response::json(201, &admin_json(&entry)).with_header("cache-control", "no-store")
}

async fn revoke_admin(app: &AdminApp, req: Request) -> Response {
    let (session, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p) {
        return r;
    }
    if req.query.is_some() {
        return err(400, "invalid");
    }
    let address = req.param("a").unwrap_or_default();
    if parse_address(address).is_err() {
        return err(404, "not_found");
    }
    let account = match app.directory.find_account(address).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    if account.id == session.account_id() {
        // Nobody removes the grant it is signed in with.
        return err(400, "invalid");
    }
    match app.admins.revoke_keeping_one(account.id).await {
        Ok(Revoked::Removed) => Response::json(200, &serde_json::json!({})).with_header("cache-control", "no-store"),
        Ok(Revoked::NotAdmin) => err(404, "not_found"),
        Ok(Revoked::LastPlatform) => err(409, "exists"),
        Err(e) => fail(&e),
    }
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/admins", handler(move |r: Request| {
        let a = a.clone();
        async move { list_admins(&a, r).await }
    }));
    let a = app.clone();
    let router = router.route(Method::Post, "/api/v1/admins", handler(move |r: Request| {
        let a = a.clone();
        async move {
            let topics = crate::live::topics_of(&r);
            let resp = grant_admin(&a, r).await;
            a.announce_after(&resp, topics);
            resp
        }
    }));
    router.route(Method::Delete, "/api/v1/admins/{a}", handler(move |r: Request| {
        let a = app.clone();
        async move {
        let topics = crate::live::topics_of(&r);
        let resp = revoke_admin(&a, r).await;
        a.announce_after(&resp, topics);
        resp
    }
    }))
}
