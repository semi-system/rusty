//! The endpoints about where a domain's message bodies are stored (docs/design-s3-per-tenant.md, section 7). Same rules as [`crate::manage`]: session and CSRF guards first, then the
//! scope of the administrator (outside it is the same `404` as a missing thing).
//!
//! * `GET .../storage`: any administrator in scope. The platform administrator sees the settings and which keys are set; a domain administrator sees only whether the domain has a store
//!   of its own, whether it is ready, and the result of its last check.
//! * `PUT .../storage`: **platform administrators only**. Gives the domain an S3 store (or changes its settings while the store is empty, or replaces the keys). The settings obey
//!   `rusty_core::storage_policy` (https, a public host, four ports, S3-valid names); the two keys are sealed and only ever overwritten, never returned.
//! * `POST .../storage/check`: platform administrators only. Queues a `storage.check` job; the mail service, which holds the unsealing key, runs it and the result shows in `GET`.
//!   A store is used only after a check passed.
//! * A deployment without `[sealing]` answers `503 unavailable` where a secret would be sealed.

use std::sync::Arc;

use rusty_core::storage_policy::{validate_tenant_s3, ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE, STORAGE_CHECK_JOB_KIND, STORE_SECRET_OWNER};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::repo::BlobStoreRow;
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp, Services};
use crate::auth::Principal;
use crate::manage::platform_only;

const MAX_KEY: usize = 256;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

fn services(app: &AdminApp) -> Result<&Services, Response> {
    app.services.as_ref().ok_or_else(|| err(503, "unavailable"))
}

fn body<T: for<'de> Deserialize<'de>>(req: &Request) -> Result<T, Response> {
    serde_json::from_slice(&req.body).map_err(|_| err(400, "invalid"))
}

fn no_query(req: &Request) -> Result<(), Response> {
    if req.query.is_some() {
        return Err(err(400, "invalid"));
    }
    Ok(())
}

async fn scoped(app: &AdminApp, p: &Principal, name: &str) -> Result<rusty_directory::DomainInfo, Response> {
    crate::manage::scoped_domain(app, p, name).await
}

#[derive(Serialize)]
struct SettingsJson {
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    path_style: bool,
}

#[derive(Serialize)]
struct StorageJson {
    kind: String,
    /// The store is used for mail: a check passed (a store that is not ready answers deliveries with a temporary failure).
    ready: bool,
    /// The fixed category of the last check (`unchecked`, `ok`, `auth`, `no-bucket`, ...), with its time.
    check_status: String,
    checked_at: Option<i64>,
    /// Platform administrators only.
    settings: Option<SettingsJson>,
    /// Whether a key is stored (it can only be overwritten, never read); platform administrators only.
    access_key_set: Option<bool>,
    secret_key_set: Option<bool>,
}

#[derive(Serialize)]
struct StorageState {
    storage: Option<StorageJson>,
}

fn settings_of(row: &BlobStoreRow) -> Option<SettingsJson> {
    let v: serde_json::Value = serde_json::from_str(&row.config).ok()?;
    let text = |n: &str| v.get(n).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    Some(SettingsJson { endpoint: text("endpoint"), region: text("region"), bucket: text("bucket"), prefix: text("prefix"), path_style: v.get("path_style").and_then(serde_json::Value::as_bool).unwrap_or(false) })
}

async fn state_of(s: &Services, row: BlobStoreRow, platform: bool) -> Result<StorageState, Response> {
    let (settings, access, secret) = if platform {
        let meta = s.sealed.meta(STORE_SECRET_OWNER, row.id.0).await.map_err(|e| fail(&e))?;
        let has = |p: &str| meta.iter().any(|m| m.purpose == p);
        (settings_of(&row), Some(has(ACCESS_KEY_PURPOSE)), Some(has(SECRET_KEY_PURPOSE)))
    } else {
        (None, None, None)
    };
    Ok(StorageState {
        storage: Some(StorageJson { ready: row.kind != "s3" || row.check_status == "ok", kind: row.kind, check_status: row.check_status, checked_at: row.checked_at, settings, access_key_set: access, secret_key_set: secret }),
    })
}

async fn storage_get(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = no_query(&req) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let row = match s.blob_stores.for_domain(dom.id).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let Some(row) = row else { return json(200, &StorageState { storage: None }) };
    match state_of(s, row, p.scope.is_platform()).await {
        Ok(st) => json(200, &st),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutStorage {
    endpoint: String,
    region: String,
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    path_style: bool,
    access_key: String,
    secret_key: String,
}

fn key_ok(k: &str) -> bool {
    !k.is_empty() && k.len() <= MAX_KEY && !k.chars().any(char::is_control)
}

async fn storage_put(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p).and(no_query(&req)) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let b: PutStorage = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let s3 = match validate_tenant_s3(&b.endpoint, &b.region, &b.bucket, &b.prefix, b.path_style) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    if !key_ok(&b.access_key) || !key_ok(&b.secret_key) {
        return err(400, "invalid");
    }
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(seal) = s.sealing.as_ref() else { return err(503, "unavailable") };
    let (row, _created) = match s.blob_stores.set_s3(dom.id, &s3).await {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    for (purpose, value) in [(ACCESS_KEY_PURPOSE, &b.access_key), (SECRET_KEY_PURPOSE, &b.secret_key)] {
        let sealed = match rusty_seal::Context::new(STORE_SECRET_OWNER, row.id.0, purpose).and_then(|c| seal.seal(&c, value.as_bytes())) {
            Ok(x) => x,
            Err(e) => return fail(&e),
        };
        if let Err(e) = s.sealed.put(STORE_SECRET_OWNER, row.id.0, purpose, &seal.id().to_hex(), &sealed).await {
            return fail(&e);
        }
    }
    // The keys changed or the store is new: it has to pass a check before it is used, so what is stored is read back from the row.
    match s.blob_stores.get(row.id).await {
        Ok(Some(row)) => match state_of(s, row, true).await {
            Ok(st) => json(200, &st),
            Err(r) => r,
        },
        Ok(None) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

async fn storage_check(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = platform_only(&p).and(no_query(&req)) {
        return r;
    }
    let dom = match scoped(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let row = match s.blob_stores.for_domain(dom.id).await {
        Ok(Some(r)) if r.kind == "s3" => r,
        Ok(_) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    if let Err(e) = s.jobs.enqueue(STORAGE_CHECK_JOB_KIND, Some(dom.id), &serde_json::json!({ "store": row.id.0 }), None).await {
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
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/storage", |a, r| storage_get(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/storage", |a, r| storage_put(&a, r).await);
    route!(router, app, Method::Post, "/api/v1/domains/{d}/storage/check", |a, r| storage_check(&a, r).await)
}
