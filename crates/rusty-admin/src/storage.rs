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

use rusty_core::storage_policy::{validate_tenant_s3, ACCESS_KEY_PURPOSE, BACKEND_SECRET_OWNER, SECRET_KEY_PURPOSE, STORAGE_CHECK_JOB_KIND, STORE_SECRET_OWNER};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::repo::{BackendRow, BackendState, BlobStoreRow};
use rusty_store::PoolPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Serialize, Deserialize)]
struct PolicyJson {
    copies: u32,
    sync_copies: u32,
    placement: String,
    read: String,
}

#[derive(Serialize)]
struct BackendJson {
    name: String,
    state: String,
    weight: u32,
    check_status: String,
    checked_at: Option<i64>,
    /// Platform administrators only.
    settings: Option<SettingsJson>,
    access_key_set: Option<bool>,
    secret_key_set: Option<bool>,
    /// How many objects the backend holds; platform administrators only.
    objects: Option<i64>,
}

/// The room of the store as the last check judged it (docs/design-storage-capacity.md).
#[derive(Serialize)]
struct CapacityJson {
    /// The room the operator declared; `null`: not declared.
    declared_bytes: Option<i64>,
    /// What the tenant's mailboxes hold, what its accounts' quotas add up to, and how many accounts have no quota; `null` before the first check.
    used_bytes: Option<i64>,
    promised_bytes: Option<i64>,
    unlimited_accounts: Option<i64>,
    /// `unset`, `ok`, `overcommitted`, `nearly_full` or `full`.
    status: String,
}

fn capacity_of(row: &BlobStoreRow) -> Option<CapacityJson> {
    (row.domain.is_some() && (row.kind == "s3" || row.kind == "pool")).then(|| CapacityJson {
        declared_bytes: row.capacity_bytes,
        used_bytes: row.used_bytes,
        promised_bytes: row.promised_bytes,
        unlimited_accounts: row.unlimited_accounts,
        status: row.capacity_status.clone(),
    })
}

#[derive(Serialize)]
struct StorageJson {
    /// The room of the store; `null` for a store that is not a tenant's own.
    capacity: Option<CapacityJson>,
    kind: String,
    /// The store is used for mail: a check passed (a store that is not ready answers deliveries with a temporary failure).
    ready: bool,
    /// The fixed category of the last check (`unchecked`, `ok`, `auth`, `no-bucket`, ...), with its time.
    check_status: String,
    checked_at: Option<i64>,
    /// Platform administrators only.
    settings: Option<SettingsJson>,
    /// Whether a key is stored (it can only be overwritten, never returned); platform administrators only.
    access_key_set: Option<bool>,
    secret_key_set: Option<bool>,
    /// A pool: how objects are spread; its backends.
    policy: Option<PolicyJson>,
    backends: Option<Vec<BackendJson>>,
}

#[derive(Serialize)]
struct StorageState {
    storage: Option<StorageJson>,
}

fn settings_text(config: &str) -> Option<SettingsJson> {
    let v: Value = serde_json::from_str(config).ok()?;
    let text = |n: &str| v.get(n).and_then(Value::as_str).unwrap_or_default().to_owned();
    Some(SettingsJson { endpoint: text("endpoint"), region: text("region"), bucket: text("bucket"), prefix: text("prefix"), path_style: v.get("path_style").and_then(Value::as_bool).unwrap_or(false) })
}

fn settings_of(row: &BlobStoreRow) -> Option<SettingsJson> {
    settings_text(&row.config)
}

async fn backend_json(s: &Services, b: BackendRow, platform: bool) -> Result<BackendJson, Response> {
    let (settings, access, secret, objects) = if platform {
        let meta = s.sealed.meta(BACKEND_SECRET_OWNER, b.id.0).await.map_err(|e| fail(&e))?;
        let has = |p: &str| meta.iter().any(|m| m.purpose == p);
        let held = s.pools.count_held(b.id).await.map_err(|e| fail(&e))?;
        (settings_text(&b.config), Some(has(ACCESS_KEY_PURPOSE)), Some(has(SECRET_KEY_PURPOSE)), Some(held))
    } else {
        (None, None, None, None)
    };
    Ok(BackendJson { name: b.name, state: b.state.name().to_owned(), weight: b.weight, check_status: b.check_status, checked_at: b.checked_at, settings, access_key_set: access, secret_key_set: secret, objects })
}

async fn state_of(s: &Services, row: BlobStoreRow, platform: bool) -> Result<StorageState, Response> {
    if row.kind == "pool" {
        let policy = PoolPolicy::parse(&row.config).ok().and_then(|p| serde_json::from_str::<PolicyJson>(&p.to_json()).ok());
        let mut backends = Vec::new();
        for b in s.pools.backends(row.id).await.map_err(|e| fail(&e))? {
            backends.push(backend_json(s, b, platform).await?);
        }
        return Ok(StorageState {
            storage: Some(StorageJson {
                capacity: capacity_of(&row),
                ready: row.check_status == "ok",
                kind: row.kind,
                check_status: row.check_status,
                checked_at: row.checked_at,
                settings: None,
                access_key_set: None,
                secret_key_set: None,
                policy,
                backends: Some(backends),
            }),
        });
    }
    let (settings, access, secret) = if platform {
        let meta = s.sealed.meta(STORE_SECRET_OWNER, row.id.0).await.map_err(|e| fail(&e))?;
        let has = |p: &str| meta.iter().any(|m| m.purpose == p);
        (settings_of(&row), Some(has(ACCESS_KEY_PURPOSE)), Some(has(SECRET_KEY_PURPOSE)))
    } else {
        (None, None, None)
    };
    Ok(StorageState {
        storage: Some(StorageJson {
            capacity: capacity_of(&row),
            ready: row.kind != "s3" || row.check_status == "ok",
            kind: row.kind,
            check_status: row.check_status,
            checked_at: row.checked_at,
            settings,
            access_key_set: access,
            secret_key_set: secret,
            policy: None,
            backends: None,
        }),
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
        Ok(Some(r)) if r.kind == "s3" || r.kind == "pool" => r,
        Ok(_) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    if let Err(e) = s.jobs.enqueue(STORAGE_CHECK_JOB_KIND, Some(dom.id), &serde_json::json!({ "store": row.id.0 }), None).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({ "queued": true }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityBody {
    /// The room of the store in bytes; `null` clears it.
    capacity_bytes: Option<i64>,
}

/// `PUT .../storage/capacity`: declare how much room the tenant's store has (the S3 protocol cannot say). A check is queued so that the verdict follows.
async fn storage_capacity(app: &AdminApp, req: Request) -> Response {
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
    let b: CapacityBody = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.capacity_bytes.is_some_and(|c| c <= 0 || c > 1 << 60) {
        return err(400, "invalid");
    }
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let row = match s.blob_stores.for_domain(dom.id).await {
        Ok(Some(r)) if r.kind == "s3" || r.kind == "pool" => r,
        Ok(_) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    if let Err(e) = s.blob_stores.set_capacity(row.id, b.capacity_bytes).await {
        return fail(&e);
    }
    if let Err(e) = s.jobs.enqueue(STORAGE_CHECK_JOB_KIND, Some(dom.id), &serde_json::json!({ "store": row.id.0 }), None).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({ "queued": true }))
}

// ------------------------------------------------------------------------------------------------ pools of backends

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutPool {
    copies: Option<u32>,
    sync_copies: Option<u32>,
    placement: Option<String>,
    read: Option<String>,
}

async fn storage_state_of_domain(s: &Services, dom: &rusty_directory::DomainInfo) -> Response {
    match s.blob_stores.for_domain(dom.id).await {
        Ok(Some(row)) => match state_of(s, row, true).await {
            Ok(st) => json(200, &st),
            Err(r) => r,
        },
        Ok(None) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

/// `PUT .../storage/pool`: make the domain's store a pool with this policy (or change the policy of its pool while it holds no body).
async fn pool_put(app: &AdminApp, req: Request) -> Response {
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
    let b: PutPool = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut m = serde_json::Map::new();
    if let Some(v) = b.copies {
        m.insert("copies".into(), v.into());
    }
    if let Some(v) = b.sync_copies {
        m.insert("sync_copies".into(), v.into());
    }
    if let Some(v) = b.placement {
        m.insert("placement".into(), v.into());
    }
    if let Some(v) = b.read {
        m.insert("read".into(), v.into());
    }
    let policy = match PoolPolicy::parse(&Value::Object(m).to_string()) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(e) = s.pools.set_policy(dom.id, policy).await {
        return fail(&e);
    }
    storage_state_of_domain(s, &dom).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBackend {
    name: String,
    endpoint: String,
    region: String,
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    path_style: bool,
    weight: Option<u32>,
    access_key: String,
    secret_key: String,
}

async fn seal_keys(s: &Services, owner: &str, id: i64, access: &str, secret: &str) -> Result<(), Response> {
    let Some(seal) = s.sealing.as_ref() else { return Err(err(503, "unavailable")) };
    for (purpose, value) in [(ACCESS_KEY_PURPOSE, access), (SECRET_KEY_PURPOSE, secret)] {
        let sealed = match rusty_seal::Context::new(owner, id, purpose).and_then(|c| seal.seal(&c, value.as_bytes())) {
            Ok(x) => x,
            Err(e) => return Err(fail(&e)),
        };
        if let Err(e) = s.sealed.put(owner, id, purpose, &seal.id().to_hex(), &sealed).await {
            return Err(fail(&e));
        }
    }
    Ok(())
}

/// `POST .../storage/backends`: add an S3 backend to the domain's pool.
async fn backend_add(app: &AdminApp, req: Request) -> Response {
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
    let b: AddBackend = match body(&req) {
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
    if s.sealing.is_none() {
        return err(503, "unavailable");
    }
    let pool = match s.pools.pool_of(dom.id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    let row = match s.pools.add_s3(pool.id, &b.name, &s3, b.weight.unwrap_or(100)).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    if let Err(r) = seal_keys(s, BACKEND_SECRET_OWNER, row.id.0, &b.access_key, &b.secret_key).await {
        // A backend without keys is of no use: take it away again rather than leave one that can never pass a check.
        let _ = s.pools.remove(row.id).await;
        return r;
    }
    storage_state_of_domain(s, &dom).await
}

async fn backend_of(s: &Services, dom: &rusty_directory::DomainInfo, name: &str) -> Result<BackendRow, Response> {
    let pool = match s.pools.pool_of(dom.id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(404, "not_found")),
        Err(e) => return Err(fail(&e)),
    };
    match s.pools.backends(pool.id).await {
        Ok(list) => list.into_iter().find(|b| b.name == name).ok_or_else(|| err(404, "not_found")),
        Err(e) => Err(fail(&e)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBackend {
    state: Option<String>,
    weight: Option<u32>,
}

/// `PUT .../storage/backends/{b}`: the state and/or the weight of a backend.
async fn backend_put(app: &AdminApp, req: Request) -> Response {
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
    let b: PutBackend = match body(&req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.state.is_none() && b.weight.is_none() {
        return err(400, "invalid");
    }
    let state = match b.state.as_deref().map(BackendState::parse) {
        None => None,
        Some(Some(st)) => Some(st),
        Some(None) => return err(400, "invalid"),
    };
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let backend = match backend_of(s, &dom, req.param("b").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Some(w) = b.weight {
        if let Err(e) = s.pools.set_weight(backend.id, w).await {
            return fail(&e);
        }
    }
    if let Some(st) = state {
        if let Err(e) = s.pools.set_state(backend.id, st).await {
            return fail(&e);
        }
    }
    storage_state_of_domain(s, &dom).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    access_key: String,
    secret_key: String,
}

/// `POST .../storage/backends/{b}/keys`: replace the keys of a backend; it has to pass a check again.
async fn backend_keys(app: &AdminApp, req: Request) -> Response {
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
    let k: Keys = match body(&req) {
        Ok(k) => k,
        Err(r) => return r,
    };
    if !key_ok(&k.access_key) || !key_ok(&k.secret_key) {
        return err(400, "invalid");
    }
    let s = match services(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if s.sealing.is_none() {
        return err(503, "unavailable");
    }
    let backend = match backend_of(s, &dom, req.param("b").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = seal_keys(s, BACKEND_SECRET_OWNER, backend.id.0, &k.access_key, &k.secret_key).await {
        return r;
    }
    let settings = match settings_text(&backend.config) {
        Some(x) => x,
        None => return err(400, "invalid"),
    };
    let s3 = match validate_tenant_s3(&settings.endpoint, &settings.region, &settings.bucket, &settings.prefix, settings.path_style) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    // The same settings again: the backend is unchecked once more.
    if let Err(e) = s.pools.set_s3(backend.id, &s3).await {
        return fail(&e);
    }
    storage_state_of_domain(s, &dom).await
}

/// `DELETE .../storage/backends/{b}`: remove a backend that holds no object (409 otherwise: drain it first).
async fn backend_delete(app: &AdminApp, req: Request) -> Response {
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
    let backend = match backend_of(s, &dom, req.param("b").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(e) = s.pools.remove(backend.id).await {
        return fail(&e);
    }
    json(200, &serde_json::json!({}))
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
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/storage", |a, r| storage_get(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/storage", |a, r| storage_put(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/storage/pool", |a, r| pool_put(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/storage/backends", |a, r| backend_add(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/storage/backends/{b}", |a, r| backend_put(&a, r).await);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/storage/backends/{b}/keys", |a, r| backend_keys(&a, r).await);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}/storage/backends/{b}", |a, r| backend_delete(&a, r).await);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/storage/capacity", |a, r| storage_capacity(&a, r).await);
    route!(router, app, Method::Post, "/api/v1/domains/{d}/storage/check", |a, r| storage_check(&a, r).await)
}
