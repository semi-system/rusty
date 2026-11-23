//! The read path of the webmail (docs/design-webmail.md section 4): the folders of the signed-in account with their counts, a page of the messages of a folder (newest first, by cursor), and one message with its
//! headers, its plain text and the list of its attachments; the original as a download.
//!
//! Every route opens **the mailbox of the account behind the session** (the hub opens it by account id) and asks it for the message: an id is the id inside that mailbox, so a message of another account is
//! not reachable by any spelling of an id. Paging is by cursor, never by page number, and every list has a limit.

use std::sync::Arc;

use rusty_core::{Error, FolderId, MessageId};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::{Mailbox, MessageSummary};
use serde::Serialize;

use crate::api::{error, WebmailApp};
use crate::mail;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;
/// A window of uids read at once while paging backwards, and the most windows one request reads.
const WINDOW: u32 = 500;
const MAX_WINDOWS: usize = 40;

fn json<T: Serialize>(v: &T) -> Response {
    Response::json(200, v).with_header("cache-control", "no-store")
}

/// Backend errors as the fixed codes of the API; the text never leaves the process.
fn fail(e: &Error) -> Response {
    match e {
        Error::NotFound(_) => error(404, "not_found"),
        Error::Invalid(_) => error(400, "invalid"),
        _ => error(503, "busy"),
    }
}

/// The mailbox of the signed-in account, and its address.
async fn mailbox(app: &WebmailApp, req: &Request) -> Result<Mailbox, Response> {
    let session = app.require(req, false).await?;
    app.hub.open_for(session.account_id()).await.map_err(|e| fail(&e))
}

#[derive(Serialize)]
struct FolderJson {
    id: i64,
    name: String,
    role: String,
    parent: Option<i64>,
    messages: u32,
    unseen: u32,
}

#[derive(Serialize)]
struct FoldersJson {
    items: Vec<FolderJson>,
}

async fn folders(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let list = match mb.list_folders().await {
        Ok(l) => l,
        Err(e) => return fail(&e),
    };
    let mut items = Vec::with_capacity(list.len());
    for f in list {
        let counts = match mb.folder_counts(f.id).await {
            Ok(c) => c,
            Err(e) => return fail(&e),
        };
        items.push(FolderJson { id: f.id.0, name: f.name, role: f.role, parent: f.parent.map(|p| p.0), messages: counts.messages, unseen: counts.unseen });
    }
    json(&FoldersJson { items })
}

#[derive(Serialize)]
struct FlagsJson {
    seen: bool,
    flagged: bool,
    answered: bool,
    draft: bool,
}

fn flags(f: rusty_core::Flags) -> FlagsJson {
    FlagsJson { seen: f.contains(rusty_core::Flags::SEEN), flagged: f.contains(rusty_core::Flags::FLAGGED), answered: f.contains(rusty_core::Flags::ANSWERED), draft: f.contains(rusty_core::Flags::DRAFT) }
}

#[derive(Serialize)]
struct ItemJson {
    id: i64,
    uid: u32,
    subject: String,
    from: String,
    /// When it arrived, unix seconds.
    date: i64,
    size: i64,
    flags: FlagsJson,
}

#[derive(Serialize)]
struct PageJson {
    items: Vec<ItemJson>,
    /// The `before` of the next page (the uid of the oldest message returned), or `null` at the end.
    next: Option<u32>,
}

fn item(m: MessageSummary) -> ItemJson {
    ItemJson { id: m.id.0, uid: m.uid, subject: mail::decode_words(&m.subject), from: mail::decode_words(&m.from_addr), date: m.internal_date, size: m.size, flags: flags(m.flags) }
}

/// The query string as (name, value) pairs; a name or value that is not plain is not understood.
fn query(req: &Request) -> Vec<(String, String)> {
    req.query.as_deref().unwrap_or_default().split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((p.to_owned(), String::new()), |(k, v)| (k.to_owned(), v.to_owned()))).collect()
}

/// `GET /api/v1/folders/{f}/messages?before=<uid>&limit=<n>`: the newest messages first.
async fn messages(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(folder) = req.param("f").and_then(|f| f.parse::<i64>().ok()).filter(|n| *n > 0) else { return error(404, "not_found") };
    let (mut before, mut limit) = (None, DEFAULT_LIMIT);
    for (k, v) in query(&req) {
        match (k.as_str(), v.parse::<u32>()) {
            ("before", Ok(n)) if n >= 1 => before = Some(n),
            ("limit", Ok(n)) if (1..=MAX_LIMIT as u32).contains(&n) => limit = n as usize,
            _ => return error(400, "invalid"),
        }
    }
    let folders = match mb.list_folders().await {
        Ok(l) => l,
        Err(e) => return fail(&e),
    };
    let Some(info) = folders.iter().find(|f| f.id.0 == folder) else { return error(404, "not_found") };
    let id = FolderId(folder);
    // Backwards from the cursor, a window of uids at a time.
    let mut hi = before.unwrap_or(info.uid_next);
    let mut out: Vec<ItemJson> = Vec::with_capacity(limit);
    let mut more = false;
    for _ in 0..MAX_WINDOWS {
        if hi <= 1 {
            break;
        }
        let lo = hi.saturating_sub(WINDOW + 1);
        let page = match mb.list_folder(id, lo, WINDOW).await {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        for m in page.into_iter().rev().filter(|m| m.uid < hi) {
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(item(m));
        }
        if more {
            break;
        }
        hi = lo + 1;
    }
    let next = if more { out.last().map(|i| i.uid) } else { None };
    json(&PageJson { items: out, next })
}

#[derive(Serialize)]
struct AttachmentJson {
    name: Option<String>,
    content_type: String,
    size: usize,
}

#[derive(Serialize)]
struct MessageJson {
    id: i64,
    folder: i64,
    date: i64,
    size: i64,
    flags: FlagsJson,
    from: String,
    to: String,
    cc: String,
    subject: String,
    message_id: String,
    /// The plain text of the message; empty when it has none.
    text: String,
    text_truncated: bool,
    /// The message has a body only as HTML, which this reader does not show yet: the original can be downloaded.
    html_only: bool,
    /// The message has an HTML part: `/api/v1/messages/{m}/html` serves it for a sandboxed frame.
    has_html: bool,
    attachments: Vec<AttachmentJson>,
}

fn message_id(req: &Request) -> Option<MessageId> {
    req.param("m").and_then(|m| m.parse::<i64>().ok()).filter(|n| *n > 0).map(MessageId)
}

/// `GET /api/v1/messages/{m}`.
async fn message(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(id) = message_id(&req) else { return error(404, "not_found") };
    let detail = match mb.get_message(id).await {
        Ok(d) if !d.deleted => d,
        Ok(_) => return error(404, "not_found"),
        Err(e) => return fail(&e),
    };
    let raw = match mb.get_raw(id).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let v = mail::view(&raw);
    json(&MessageJson {
        id: id.0,
        folder: detail.folder.0,
        date: detail.summary.internal_date,
        size: detail.summary.size,
        flags: flags(detail.summary.flags),
        from: v.headers.from,
        to: v.headers.to,
        cc: v.headers.cc,
        subject: v.headers.subject,
        message_id: v.headers.message_id,
        text: v.text,
        text_truncated: v.text_truncated,
        html_only: v.html_only,
        has_html: v.html.is_some(),
        attachments: v.attachments.into_iter().map(|a| AttachmentJson { name: a.name, content_type: a.content_type, size: a.size }).collect(),
    })
}

/// `GET /api/v1/messages/{m}/raw`: the message as it arrived, always as a download and never as a page of this origin.
async fn raw(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(id) = message_id(&req) else { return error(404, "not_found") };
    match mb.get_raw(id).await {
        Ok(bytes) => Response::new(200)
            .with_header("content-type", "application/octet-stream")
            .with_header("content-disposition", format!("attachment; filename=\"message-{}.eml\"", id.0))
            .with_header("x-content-type-options", "nosniff")
            .with_header("content-security-policy", "sandbox; default-src 'none'")
            .with_header("cache-control", "no-store")
            .with_body(bytes),
        Err(e) => fail(&e),
    }
}

/// What the browser is told about the HTML of a message, which is hostile content: no script (the sandbox has no `allow-scripts`), nothing loaded from anywhere (so no tracking pixel, no remote
/// style, no font), inline styles and `data:` images only, no form, no base, and it may be framed by this origin alone. Links open in a new window that is not sandboxed and sends no referrer.
const HTML_CSP: &str = "sandbox allow-popups allow-popups-to-escape-sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

/// `GET /api/v1/messages/{m}/html`: the HTML part, for the sandboxed frame of the reader and for nothing else.
async fn html(app: &WebmailApp, req: Request) -> Response {
    let mb = match mailbox(app, &req).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Some(id) = message_id(&req) else { return error(404, "not_found") };
    let raw = match mb.get_raw(id).await {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let Some(body) = mail::view(&raw).html else { return error(404, "not_found") };
    let page = format!("<!doctype html><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\"><base target=\"_blank\">{body}");
    Response::new(200)
        .with_header("content-type", "text/html; charset=utf-8")
        .with_header("content-security-policy", HTML_CSP)
        .with_header("x-content-type-options", "nosniff")
        .with_header("referrer-policy", "no-referrer")
        .with_header("cache-control", "no-store")
        .with_body(page.into_bytes())
}

pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    macro_rules! route {
        ($router:expr, $path:expr, $f:ident) => {{
            let a = app.clone();
            $router.route(Method::Get, $path, handler(move |r: Request| {
                let a = a.clone();
                async move { $f(&a, r).await }
            }))
        }};
    }
    let router = route!(router, "/api/v1/folders", folders);
    let router = route!(router, "/api/v1/folders/{f}/messages", messages);
    let router = route!(router, "/api/v1/messages/{m}", message);
    let router = route!(router, "/api/v1/messages/{m}/raw", raw);
    route!(router, "/api/v1/messages/{m}/html", html)
}
