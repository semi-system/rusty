//! The write path of the webmail, first part (docs/design-webmail.md section 5): organising the mail that is already there. Mark, flag, move, copy and delete messages; create, rename and delete folders; empty
//! the Trash or the Junk folder. Composing and sending is a later step.
//!
//! Every route needs the CSRF token and the Origin check (`require(.., true)`) and works on **the mailbox of the account behind the session**, like the read path: an id of another account is "missing",
//! never changed. A request names at most [`MAX_IDS`] messages.

use std::sync::Arc;

use rusty_core::{Error, Flags, FolderId, MessageId};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::Mailbox;
use serde::{Deserialize, Serialize};

use crate::api::{error, WebmailApp};

const MAX_IDS: usize = 100;
/// The most rounds of "empty a folder" in one request (500 messages each).
const EMPTY_ROUNDS: usize = 20;

fn json<T: Serialize>(v: &T) -> Response {
    Response::json(200, v).with_header("cache-control", "no-store")
}

fn fail(e: &Error) -> Response {
    match e {
        Error::NotFound(_) => error(404, "not_found"),
        Error::Invalid(_) => error(400, "invalid"),
        Error::Conflict(_) => error(409, "conflict"),
        _ => error(503, "busy"),
    }
}

async fn mailbox(app: &WebmailApp, req: &Request) -> Result<Mailbox, Response> {
    let session = app.require(req, true).await?;
    app.hub.open_for(session.account_id()).await.map_err(|e| fail(&e))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionBody {
    ids: Vec<i64>,
    action: String,
    /// The destination folder of `move` and `copy`.
    folder: Option<i64>,
}

#[derive(Serialize)]
struct ActionResult {
    applied: usize,
    /// Ids that are not messages of this mailbox (or are gone).
    missing: Vec<i64>,
}

fn ids(v: &[i64]) -> Option<Vec<MessageId>> {
    (!v.is_empty() && v.len() <= MAX_IDS && v.iter().all(|i| *i > 0)).then(|| v.iter().map(|i| MessageId(*i)).collect())
}

fn missing(v: Vec<MessageId>) -> Vec<i64> {
    v.into_iter().map(|m| m.0).collect()
}

async fn flags(mb: &Mailbox, list: &[MessageId], add: Flags, remove: Flags) -> rusty_core::Result<ActionResult> {
    let u = mb.update_flags_many(list, add, remove).await?;
    Ok(ActionResult { applied: u.updated.len(), missing: missing(u.missing) })
}

/// `POST /api/v1/messages/actions`: `{ "ids": [..], "action": "read|unread|flag|unflag|move|copy|delete", "folder": n }`.
/// `delete` moves to the Trash folder, and removes for good a message that already is in it.
async fn actions(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<ActionBody>(&req.body) else { return error(400, "invalid") };
    let Some(list) = ids(&b.ids) else { return error(400, "invalid") };
    let out = match (b.action.as_str(), b.folder) {
        ("read", None) => flags(&mb, &list, Flags::SEEN, Flags::empty()).await,
        ("unread", None) => flags(&mb, &list, Flags::empty(), Flags::SEEN).await,
        ("flag", None) => flags(&mb, &list, Flags::FLAGGED, Flags::empty()).await,
        ("unflag", None) => flags(&mb, &list, Flags::empty(), Flags::FLAGGED).await,
        ("move", Some(f)) if f > 0 => mb.move_many(&list, FolderId(f)).await.map(|m| ActionResult { applied: m.moved.len(), missing: missing(m.missing) }),
        ("copy", Some(f)) if f > 0 => mb.copy_many(&list, FolderId(f)).await.map(|m| ActionResult { applied: m.copied.len(), missing: missing(m.missing) }),
        ("delete", None) => delete(&mb, &list).await,
        _ => return error(400, "invalid"),
    };
    match out {
        Ok(r) => json(&r),
        Err(e) => fail(&e),
    }
}

async fn delete(mb: &Mailbox, list: &[MessageId]) -> rusty_core::Result<ActionResult> {
    let folders = mb.list_folders().await?;
    let trash = folders.iter().find(|f| f.role == "trash").map(|f| f.id);
    // Those already in the Trash go for good; the others go to it. Without a Trash folder, everything goes for good.
    let mut gone = Vec::new();
    let mut to_trash = Vec::new();
    for id in list {
        match (mb.get_message(*id).await, trash) {
            (Ok(d), Some(t)) if d.folder != t => to_trash.push(*id),
            (Ok(_), _) | (Err(Error::NotFound(_)), _) => gone.push(*id),
            (Err(e), _) => return Err(e),
        }
    }
    let (mut applied, mut miss) = (0, Vec::new());
    if let (false, Some(t)) = (to_trash.is_empty(), trash) {
        let m = mb.move_many(&to_trash, t).await?;
        applied += m.moved.len();
        miss.extend(missing(m.missing));
    }
    if !gone.is_empty() {
        let o = mb.expunge_many(&gone).await?;
        applied += o.applied as usize;
        miss.extend(missing(o.missing));
    }
    Ok(ActionResult { applied, missing: miss })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderBody {
    name: String,
    parent: Option<i64>,
}

#[derive(Serialize)]
struct Created {
    id: i64,
}

fn folder_id(req: &Request) -> Option<FolderId> {
    req.param("f").and_then(|f| f.parse::<i64>().ok()).filter(|n| *n > 0).map(FolderId)
}

/// `POST /api/v1/folders`: `{ "name": "..", "parent": n }`.
async fn create_folder(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<FolderBody>(&req.body) else { return error(400, "invalid") };
    match mb.create_folder(b.parent.filter(|p| *p > 0).map(FolderId), &b.name).await {
        Ok(id) => json(&Created { id: id.0 }),
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

/// `POST /api/v1/folders/{f}/rename`: `{ "name": ".." }`.
async fn rename_folder(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(id) = folder_id(&req) else { return error(404, "not_found") };
    let Ok(b) = serde_json::from_slice::<RenameBody>(&req.body) else { return error(400, "invalid") };
    match mb.rename_folder(id, &b.name).await {
        Ok(()) => json(&serde_json::json!({})),
        Err(e) => fail(&e),
    }
}

/// `DELETE /api/v1/folders/{f}`: a system folder is refused by the mailbox.
async fn delete_folder(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(id) = folder_id(&req) else { return error(404, "not_found") };
    match mb.delete_folder(id).await {
        Ok(()) => json(&serde_json::json!({})),
        Err(e) => fail(&e),
    }
}

/// `POST /api/v1/folders/{f}/empty`: removes every message of the Trash or the Junk folder, and of no other.
async fn empty_folder(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(id) = folder_id(&req) else { return error(404, "not_found") };
    let folders = match mb.list_folders().await {
        Ok(l) => l,
        Err(e) => return fail(&e),
    };
    match folders.iter().find(|f| f.id == id) {
        Some(f) if f.role == "trash" || f.role == "spam" => {}
        Some(_) => return error(400, "invalid"),
        None => return error(404, "not_found"),
    }
    let mut removed = 0usize;
    for _ in 0..EMPTY_ROUNDS {
        let page = match mb.list_folder(id, 0, 500).await {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        if page.is_empty() {
            break;
        }
        let list: Vec<MessageId> = page.iter().map(|m| m.id).collect();
        match mb.expunge_many(&list).await {
            Ok(o) => removed += o.applied as usize,
            Err(e) => return fail(&e),
        }
    }
    json(&serde_json::json!({ "removed": removed }))
}

pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    macro_rules! route {
        ($router:expr, $method:expr, $path:expr, $f:ident) => {{
            let a = app.clone();
            $router.route($method, $path, handler(move |r: Request| {
                let a = a.clone();
                async move { $f(&a, r).await }
            }))
        }};
    }
    let router = route!(router, Method::Post, "/api/v1/messages/actions", actions);
    let router = route!(router, Method::Post, "/api/v1/folders", create_folder);
    let router = route!(router, Method::Post, "/api/v1/folders/{f}/rename", rename_folder);
    let router = route!(router, Method::Post, "/api/v1/folders/{f}/empty", empty_folder);
    route!(router, Method::Delete, "/api/v1/folders/{f}", delete_folder)
}
