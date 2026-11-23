//! The write path of the webmail, second part (docs/design-webmail.md, sending): `POST /api/v1/send` composes a plain-text message from the form, stores it and queues it in the outbox; the submission service
//! sends it through the same steps as an authenticated SMTP submission. `GET /api/v1/outbox` lists what was queued and how it went; `DELETE /api/v1/outbox/{id}` withdraws a message that was not taken yet
//! (or that failed). The answer to a send is `202` as soon as the message is queued: whether it left is read from the outbox.

use std::sync::Arc;

use bytes::Bytes;
use rusty_core::Error;
use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};

use crate::api::{error, WebmailApp};
use crate::compose::{self, Draft, Invalid};

const OUTBOX_PAGE: i64 = 50;

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBody {
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    text: String,
    in_reply_to: Option<String>,
    /// The draft this message was made from: it is removed once the message is queued.
    draft_id: Option<i64>,
}

#[derive(Serialize)]
struct Queued {
    id: i64,
}

/// `POST /api/v1/send`.
async fn send(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<SendBody>(&req.body) else { return error(400, "invalid") };
    let draft_id = b.draft_id;
    let draft = Draft { to: b.to, cc: b.cc, bcc: b.bcc, subject: b.subject, text: b.text, in_reply_to: b.in_reply_to };
    let composed = match compose::build(&session.address, &draft, compose::now()) {
        Ok(c) => c,
        Err(Invalid::NoRecipient) => return error(400, "no_recipient"),
        Err(Invalid::TooManyRecipients) => return error(400, "too_many_recipients"),
        Err(Invalid::Address) => return error(400, "bad_address"),
        Err(Invalid::Subject | Invalid::Reference) => return error(400, "invalid"),
    };
    if composed.bytes.len() > app.max_message_bytes {
        return error(413, "too_large");
    }
    let size = i64::try_from(composed.bytes.len()).unwrap_or(i64::MAX);
    let blob = match app.blobs.put(Bytes::from(composed.bytes)).await {
        Ok(h) => h,
        Err(_) => return error(503, "busy"),
    };
    match app.outbox.enqueue(session.account_id(), &session.address, blob.as_str(), size, &composed.recipients, &composed.subject).await {
        Ok(id) => {
            if let Some(d) = draft_id {
                remove_draft(app, session.account_id(), d).await;
            }
            json(202, &Queued { id })
        }
        Err(Error::Quota(_)) => error(429, "outbox_full"),
        Err(_) => error(503, "busy"),
    }
}

#[derive(Serialize)]
struct OutboxJson {
    id: i64,
    subject: String,
    recipients: Vec<String>,
    /// `queued`, `sending`, `sent` or `failed`.
    state: String,
    /// A fixed word when it failed (`not_yours`, `limit`, `refused`, `busy`, `failed`).
    error: Option<String>,
    created: i64,
    finished: Option<i64>,
}

#[derive(Serialize)]
struct OutboxList {
    items: Vec<OutboxJson>,
}

/// `GET /api/v1/outbox`: the newest messages of the account, the most 50.
async fn outbox(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.outbox.list(session.account_id(), OUTBOX_PAGE).await {
        Ok(items) => json(
            200,
            &OutboxList {
                items: items
                    .into_iter()
                    .map(|i| OutboxJson { id: i.id, subject: i.subject, recipients: i.recipients, state: i.state, error: i.error, created: i.created_at, finished: i.finished_at })
                    .collect(),
            },
        ),
        Err(_) => error(503, "busy"),
    }
}

/// `DELETE /api/v1/outbox/{id}`.
async fn discard(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(id) = req.param("id").and_then(|i| i.parse::<i64>().ok()).filter(|n| *n > 0) else { return error(404, "not_found") };
    match app.outbox.discard(session.account_id(), id).await {
        Ok(true) => json(200, &serde_json::json!({})),
        Ok(false) => error(404, "not_found"),
        Err(_) => error(503, "busy"),
    }
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
    let router = route!(router, Method::Post, "/api/v1/send", send);
    let router = route!(router, Method::Post, "/api/v1/drafts", save_draft);
    let router = route!(router, Method::Get, "/api/v1/outbox", outbox);
    route!(router, Method::Delete, "/api/v1/outbox/{id}", discard)
}

/// The Drafts folder of the account, and the draft `id` in it if it is one.
async fn drafts_of(app: &WebmailApp, account: rusty_core::AccountId) -> Option<(rusty_store::Mailbox, rusty_core::FolderId)> {
    let mb = app.hub.open_for(account).await.ok()?;
    let folder = mb.list_folders().await.ok()?.into_iter().find(|f| f.role == "drafts")?.id;
    Some((mb, folder))
}

/// Remove a draft that was sent or replaced. Only a message of the Drafts folder is removed, whatever id the request carries; a failure leaves the draft, which is the safe side.
async fn remove_draft(app: &WebmailApp, account: rusty_core::AccountId, id: i64) {
    let Some((mb, folder)) = drafts_of(app, account).await else { return };
    let id = rusty_core::MessageId(id);
    if matches!(mb.get_message(id).await, Ok(d) if d.folder == folder && !d.deleted) {
        let _ = mb.expunge(id).await;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftBody {
    /// The draft this one replaces.
    id: Option<i64>,
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    text: String,
    in_reply_to: Option<String>,
}

/// `POST /api/v1/drafts`: keep what is typed in the Drafts folder (replacing the draft `id` when there is one); answers the id of the new draft.
async fn save_draft(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<DraftBody>(&req.body) else { return error(400, "invalid") };
    let replaces = b.id;
    let draft = Draft { to: b.to, cc: b.cc, bcc: b.bcc, subject: b.subject, text: b.text, in_reply_to: b.in_reply_to };
    let composed = match compose::build_draft(&session.address, &draft, compose::now()) {
        Ok(c) => c,
        Err(Invalid::NoRecipient) => return error(400, "no_recipient"),
        Err(Invalid::TooManyRecipients) => return error(400, "too_many_recipients"),
        Err(Invalid::Address) => return error(400, "bad_address"),
        Err(Invalid::Subject | Invalid::Reference) => return error(400, "invalid"),
    };
    if composed.bytes.len() > app.max_message_bytes {
        return error(413, "too_large");
    }
    let Some((mb, folder)) = drafts_of(app, session.account_id()).await else { return error(503, "busy") };
    let kept = match mb.append(folder, Bytes::from(composed.bytes), rusty_core::Flags::DRAFT | rusty_core::Flags::SEEN).await {
        Ok(r) => r,
        Err(Error::Quota(_)) => return error(413, "too_large"),
        Err(_) => return error(503, "busy"),
    };
    if let Some(old) = replaces {
        remove_draft(app, session.account_id(), old).await;
    }
    json(200, &Queued { id: kept.id.0 })
}
