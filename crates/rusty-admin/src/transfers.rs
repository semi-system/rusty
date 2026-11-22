//! Export and import of one mailbox in the admin web (docs/design-mailbox-transfers.md). Only platform administrators may, every request is in the audit log (a download is written by hand: it is a
//! read), and the admin service never reads a mailbox: it asks for an export and streams the file the IMAP service wrote into the database, and it uploads an import in pieces for the IMAP service to read.

use std::sync::Arc;

use bytes::Bytes;
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{NewAuditRow, Transfer};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::api::{fail, AdminApp};
use crate::manage::{platform_only, scoped_account};

/// The most an uploaded import may be (the IMAP service reads it whole).
pub const MAX_IMPORT_BYTES: i64 = 64 * 1024 * 1024;
/// The largest piece of an upload: the request body limit of the server is 64 KiB.
pub const MAX_PIECE: usize = 48 * 1024;
/// The route of a piece of an upload: a request of its own, so the audit log does not get one row for each of hundreds of pieces (the import itself is recorded when it starts).
pub const CHUNK_ROUTE: &str = "/api/v1/transfers/{t}/chunks/{n}";

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct TransferJson {
    id: i64,
    /// `export` or `import`.
    kind: String,
    requested_by: String,
    /// `uploading`, `queued`, `running`, `done` or `failed`.
    state: String,
    folder: Option<String>,
    size: i64,
    messages: i64,
    skipped: i64,
    /// A fixed word when it failed (`quota`, `too_large`, `unreadable`, `not_found`, `failed`).
    error: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
    /// It is removed from the server at this time (seconds).
    expires_at: i64,
}

#[derive(Serialize)]
struct TransferList {
    items: Vec<TransferJson>,
}

fn transfer_json(t: &Transfer) -> TransferJson {
    TransferJson {
        id: t.id,
        kind: t.kind.clone(),
        requested_by: t.requested_by.clone(),
        state: t.state.clone(),
        folder: t.folder.clone(),
        size: t.size,
        messages: t.messages,
        skipped: t.skipped,
        error: t.error.clone(),
        created_at: t.created_at,
        finished_at: t.finished_at,
        expires_at: t.expires_at,
    }
}

macro_rules! begin {
    ($app:expr, $req:expr, $mutating:expr) => {{
        let (session, p) = match $app.require(&$req, $mutating).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        if let Err(r) = platform_only(&p) {
            return r;
        }
        let Some(services) = $app.services.as_ref() else { return err(503, "unavailable") };
        (session, p, services)
    }};
}

/// `POST /api/v1/accounts/{a}/exports`: ask for the export of the whole mailbox as one mbox file.
async fn start_export(app: &AdminApp, req: Request) -> Response {
    let (session, p, s) = begin!(app, req, true);
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match s.transfers.create("export", acc.id.0, &session.address, None).await {
        Ok(id) => got(s, id).await,
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewImport {
    /// The folder messages go to when the file does not say (the Inbox when `null`).
    #[serde(default)]
    folder: Option<String>,
}

/// `POST /api/v1/accounts/{a}/imports`: begin an import; the file is uploaded in pieces and then started.
async fn start_import(app: &AdminApp, req: Request) -> Response {
    let (session, p, s) = begin!(app, req, true);
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let body: NewImport = if req.body.is_empty() { NewImport { folder: None } } else { match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => return err(400, "invalid"),
    } };
    if body.folder.as_deref().is_some_and(|f| f.trim().is_empty() || f.len() > 200 || f.chars().any(char::is_control)) {
        return err(400, "invalid");
    }
    match s.transfers.create("import", acc.id.0, &session.address, body.folder.as_deref().map(str::trim)).await {
        Ok(id) => got(s, id).await,
        Err(e) => fail(&e),
    }
}

async fn got(s: &crate::api::Services, id: i64) -> Response {
    match s.transfers.get(id).await {
        Ok(Some(t)) => Response::json(201, &transfer_json(&t)).with_header("cache-control", "no-store"),
        Ok(None) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

fn id_of(req: &Request, name: &str) -> Option<i64> {
    req.param(name).and_then(|v| v.parse::<i64>().ok()).filter(|n| *n >= 0)
}

/// `PUT /api/v1/transfers/{t}/chunks/{n}`: piece `n` of an import's file (the body, at most 48 KiB; send it with `Content-Type: application/json` like every change, it is not read as JSON). Pieces may come in any order; the import is started when all have arrived.
async fn put_chunk(app: &AdminApp, req: Request) -> Response {
    let (_, _, s) = begin!(app, req, true);
    let (Some(id), Some(n)) = (id_of(&req, "t"), id_of(&req, "n")) else { return err(400, "invalid") };
    if req.body.is_empty() || req.body.len() > MAX_PIECE || n > 100_000 {
        return err(400, "invalid");
    }
    match s.transfers.get(id).await {
        Ok(Some(t)) if t.kind == "import" => {}
        Ok(_) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    }
    match s.transfers.add_chunk(id, i32::try_from(n).unwrap_or(i32::MAX), &req.body, MAX_IMPORT_BYTES).await {
        Ok(true) => Response::json(200, &serde_json::json!({})).with_header("cache-control", "no-store"),
        // Not taking pieces any more, or the file would pass the limit.
        Ok(false) => err(409, "wrong_state"),
        Err(rusty_core::Error::Conflict(_)) => err(409, "exists"),
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    /// How many pieces the file has: it is started only when every one of `0..pieces` has arrived.
    pieces: i32,
}

/// `POST /api/v1/transfers/{t}/start`: the file has arrived; queue the import.
async fn start(app: &AdminApp, req: Request) -> Response {
    let (_, _, s) = begin!(app, req, true);
    let Some(id) = id_of(&req, "t") else { return err(400, "invalid") };
    let Ok(b) = serde_json::from_slice::<Start>(&req.body) else { return err(400, "invalid") };
    if b.pieces < 1 {
        return err(400, "invalid");
    }
    for n in 0..b.pieces {
        match s.transfers.chunk(id, n).await {
            Ok(Some(_)) => {}
            Ok(None) => return err(409, "wrong_state"),
            Err(e) => return fail(&e),
        }
    }
    // A piece beyond the count would be part of the file as well: refuse an upload that has more than it says.
    match s.transfers.chunk(id, b.pieces).await {
        Ok(None) => {}
        Ok(Some(_)) => return err(409, "wrong_state"),
        Err(e) => return fail(&e),
    }
    match s.transfers.queue_import(id).await {
        Ok(true) => match s.transfers.get(id).await {
            Ok(Some(t)) => Response::json(200, &transfer_json(&t)).with_header("cache-control", "no-store"),
            Ok(None) => err(404, "not_found"),
            Err(e) => fail(&e),
        },
        Ok(false) => err(409, "wrong_state"),
        Err(e) => fail(&e),
    }
}

/// `GET /api/v1/accounts/{a}/transfers`: the newest exports and imports of the account (they are kept a day).
async fn list(app: &AdminApp, req: Request) -> Response {
    let (_, p, s) = begin!(app, req, false);
    let (acc, _) = match scoped_account(app, &p, req.param("a").unwrap_or_default()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match s.transfers.list_for_account(acc.id.0, 20).await {
        Ok(rows) => Response::json(200, &TransferList { items: rows.iter().map(transfer_json).collect() }).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

/// `GET /api/v1/transfers/{t}`: one transfer, for the page that waits for it.
async fn get_one(app: &AdminApp, req: Request) -> Response {
    let (_, _, s) = begin!(app, req, false);
    let Some(id) = id_of(&req, "t") else { return err(400, "invalid") };
    match s.transfers.get(id).await {
        Ok(Some(t)) => Response::json(200, &transfer_json(&t)).with_header("cache-control", "no-store"),
        Ok(None) => err(404, "not_found"),
        Err(e) => fail(&e),
    }
}

/// A file name for the download.
fn file_name(account: i64) -> String {
    format!("mailbox-{account}.mbox")
}

/// `GET /api/v1/transfers/{t}/download`: the file of a finished export, as it is read from the database. Reading someone's mail is written to the audit log here, by hand (a read is not a change, so
/// the wrapper of the audit log does not see it).
async fn download(app: &AdminApp, req: Request) -> Response {
    let (session, _, s) = begin!(app, req, false);
    let Some(id) = id_of(&req, "t") else { return err(400, "invalid") };
    let t = match s.transfers.get(id).await {
        Ok(Some(t)) if t.kind == "export" && t.state == "done" => t,
        Ok(Some(_)) => return err(409, "wrong_state"),
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    let row = NewAuditRow {
        actor_id: Some(session.account),
        actor: session.address.clone(),
        domain_id: None,
        ip: req.peer.addr.ip().to_string(),
        method: "POST".to_owned(),
        route: "/api/v1/transfers/{t}/download".to_owned(),
        path: format!("/api/v1/transfers/{id}/download"),
        status: 200,
        detail: None,
    };
    if let Err(e) = s.audit.record(&row).await {
        // Mail is not handed over without the record of who took it.
        tracing::warn!(error = %e, "the audit log could not be written; the download is refused");
        return err(503, "busy");
    }
    let repo = s.transfers.clone();
    let (out, body) = mpsc::channel::<Bytes>(2);
    tokio::spawn(async move {
        let mut seq = 0i32;
        while let Ok(Some(piece)) = repo.chunk(t.id, seq).await {
            if out.send(Bytes::from(piece)).await.is_err() {
                break;
            }
            seq += 1;
        }
    });
    let name = file_name(t.account);
    Response::streaming(200, body)
        .with_header("content-type", "application/mbox")
        .with_header("content-disposition", format!("attachment; filename=\"{name}\""))
        .with_header("cache-control", "no-store")
        .with_header("x-content-type-options", "nosniff")
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    macro_rules! route {
        ($router:expr, $method:expr, $path:expr, $f:ident) => {{
            let a = app.clone();
            $router.route($method, $path, handler(move |r: Request| {
                let a = a.clone();
                async move { $f(&a, r).await }
            }))
        }};
    }
    let router = route!(router, Method::Post, "/api/v1/accounts/{a}/exports", start_export);
    let router = route!(router, Method::Post, "/api/v1/accounts/{a}/imports", start_import);
    let router = route!(router, Method::Get, "/api/v1/accounts/{a}/transfers", list);
    let router = route!(router, Method::Put, "/api/v1/transfers/{t}/chunks/{n}", put_chunk);
    let router = route!(router, Method::Post, "/api/v1/transfers/{t}/start", start);
    let router = route!(router, Method::Get, "/api/v1/transfers/{t}", get_one);
    route!(router, Method::Get, "/api/v1/transfers/{t}/download", download)
}
