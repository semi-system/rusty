//! The Backups page of the admin web (docs/design-backup-status.md): what the deployment tool did about backups, newest first: each backup (when, how big), each drill (a dump restored into a scratch database
//! and read: whether it worked and what it held) and each restore. The tool writes the rows (`rusty-swarm backup`, `backup verify`); the admin web only reads them, for platform administrators.
//!
//! The page and the answer say how old the newest good backup is and whether the newest drill passed, so that "the nightly backup stopped three days ago" is a thing one sees, not one finds out.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::BackupRun;
use serde::Serialize;

use crate::api::{fail, AdminApp};

const LIMIT: i64 = 60;

#[derive(Serialize)]
struct RunJson {
    id: i64,
    at: i64,
    kind: String,
    ok: bool,
    bytes: Option<i64>,
    detail: String,
}

#[derive(Serialize)]
struct BackupsJson {
    /// The server's clock, so that the age of a backup does not depend on the browser's.
    now: i64,
    items: Vec<RunJson>,
}

fn run(r: BackupRun) -> RunJson {
    RunJson { id: r.id, at: r.at, kind: r.kind, ok: r.ok, bytes: r.bytes, detail: r.detail }
}

/// `GET /api/v1/backups`: the newest 60 runs, newest first. Platform administrators only.
async fn list(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !p.scope.is_platform() {
        return Response::error(403, "forbidden").with_header("cache-control", "no-store");
    }
    let Some(services) = app.services.as_ref() else { return Response::error(503, "busy").with_header("cache-control", "no-store") };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    match services.backups.list(LIMIT).await {
        Ok(rows) => Response::json(200, &BackupsJson { now, items: rows.into_iter().map(run).collect() }).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    router.route(Method::Get, "/api/v1/backups", handler(move |req| {
        let app = app.clone();
        async move { list(&app, req).await }
    }))
}
