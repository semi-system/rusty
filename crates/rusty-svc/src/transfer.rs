//! The worker of the mailbox exports and imports asked for in the admin web (docs/design-mailbox-transfers.md). It runs where the mailboxes are (the IMAP service), because the admin web has no
//! access to mail: it takes the oldest queued transfer from the database, reads or writes the mailbox, and passes the file to or from the admin web in pieces through the database.

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::mbox;
use rusty_core::{AccountId, Error, Flags, FolderId, Result};
use rusty_store::repo::Transfer;
use rusty_store::{Mailbox, MAX_TRANSFER_BYTES};
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::service::Service;

/// How much of an export is held in memory before a piece is stored.
const PIECE: usize = 4 * 1024 * 1024;
/// The most an import may be (it is read whole).
pub const MAX_IMPORT_BYTES: i64 = 64 * 1024 * 1024;
/// A transfer still `running` this long after it was asked for was left by a worker that stopped.
const STUCK_AFTER_SECS: i64 = 3600;

pub struct TransferService {
    poll: Duration,
}

impl TransferService {
    pub fn new() -> TransferService {
        TransferService { poll: Duration::from_secs(2) }
    }

    pub fn with_poll(mut self, poll: Duration) -> TransferService {
        self.poll = poll;
        self
    }

    /// Run the oldest queued transfer, if any; whether there was one.
    pub async fn run_once(ctx: &Context) -> Result<bool> {
        let Some(t) = ctx.transfers.claim().await? else { return Ok(false) };
        let outcome = match t.kind.as_str() {
            "export" => export(ctx, &t).await,
            _ => import(ctx, &t).await,
        };
        match outcome {
            Ok(done) => ctx.transfers.finish(t.id, "done", done.size, done.messages, done.skipped, None).await?,
            Err((word, done)) => ctx.transfers.finish(t.id, "failed", done.size, done.messages, done.skipped, Some(word)).await?,
        }
        Ok(true)
    }
}

impl Default for TransferService {
    fn default() -> TransferService {
        TransferService::new()
    }
}

#[derive(Default)]
struct Done {
    size: i64,
    messages: i64,
    skipped: i64,
}

/// A failure as a fixed word (it is shown to the administrator and never carries text from the mailbox or the file) with what was done before it.
type Failed = (&'static str, Done);

fn word_of(e: &Error) -> &'static str {
    match e {
        Error::Quota(_) => "quota",
        Error::NotFound(_) => "not_found",
        _ => "failed",
    }
}

async fn open(ctx: &Context, t: &Transfer) -> std::result::Result<Mailbox, Failed> {
    ctx.hub.open_for(AccountId(t.account)).await.map_err(|e| (word_of(&e), Done::default()))
}

/// Every folder of the mailbox into one mbox, each message marked with its folder; the pieces go to the database as the file grows.
async fn export(ctx: &Context, t: &Transfer) -> std::result::Result<Done, Failed> {
    let mb = open(ctx, t).await?;
    let folders = mb.list_folders().await.map_err(|e| (word_of(&e), Done::default()))?;
    let (mut done, mut buf, mut seq) = (Done::default(), Vec::<u8>::new(), 0i32);
    for f in &folders {
        let mut after = 0u32;
        loop {
            let page = mb.list_folder(f.id, after, 500).await.map_err(|e| (word_of(&e), Done { ..done }))?;
            let Some(last) = page.last() else { break };
            after = last.uid;
            for m in &page {
                // A message that cannot be read (its body is gone) is counted and skipped, not a reason to lose the rest.
                match mb.get_raw(m.id).await {
                    Ok(raw) => {
                        mbox::write_message_in(&mut buf, m.internal_date, m.flags, Some(&f.name), &raw);
                        done.messages += 1;
                    }
                    Err(_) => done.skipped += 1,
                }
                if buf.len() >= PIECE {
                    flush(ctx, t, &mut buf, &mut seq, &mut done).await?;
                }
            }
        }
    }
    flush(ctx, t, &mut buf, &mut seq, &mut done).await?;
    Ok(done)
}

async fn flush(ctx: &Context, t: &Transfer, buf: &mut Vec<u8>, seq: &mut i32, done: &mut Done) -> std::result::Result<(), Failed> {
    if buf.is_empty() {
        return Ok(());
    }
    let len = i64::try_from(buf.len()).unwrap_or(i64::MAX);
    match ctx.transfers.add_chunk(t.id, *seq, buf, MAX_TRANSFER_BYTES).await {
        Ok(true) => {
            *seq += 1;
            done.size += len;
            buf.clear();
            Ok(())
        }
        Ok(false) => Err(("too_large", Done { size: done.size, messages: done.messages, skipped: done.skipped })),
        Err(e) => Err((word_of(&e), Done { size: done.size, messages: done.messages, skipped: done.skipped })),
    }
}

/// The folder named `name` (its name or its role, in any case) or a new one; `None` for a name that cannot be a folder.
async fn folder_for(mb: &Mailbox, known: &mut Vec<(String, FolderId)>, name: &str) -> Option<FolderId> {
    let wanted = name.trim();
    if let Some((_, id)) = known.iter().find(|(n, _)| n.eq_ignore_ascii_case(wanted)) {
        return Some(*id);
    }
    if wanted.is_empty() || wanted.len() > 200 || wanted.chars().any(char::is_control) {
        return None;
    }
    let id = mb.create_folder(None, wanted).await.ok()?;
    known.push((wanted.to_owned(), id));
    Some(id)
}

/// The messages of the uploaded mbox into the mailbox: each into the folder its `X-Rusty-Folder` header names, else the transfer's folder, else the Inbox, with its date and flags.
async fn import(ctx: &Context, t: &Transfer) -> std::result::Result<Done, Failed> {
    let mb = open(ctx, t).await?;
    let mut bytes = Vec::new();
    let mut seq = 0i32;
    while let Some(piece) = ctx.transfers.chunk(t.id, seq).await.map_err(|e| (word_of(&e), Done::default()))? {
        bytes.extend_from_slice(&piece);
        seq += 1;
        if i64::try_from(bytes.len()).unwrap_or(i64::MAX) > MAX_IMPORT_BYTES {
            return Err(("too_large", Done::default()));
        }
    }
    let folders = mb.list_folders().await.map_err(|e| (word_of(&e), Done::default()))?;
    let mut known: Vec<(String, FolderId)> = folders.iter().flat_map(|f| [(f.name.clone(), f.id), (f.role.clone(), f.id)]).collect();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let messages = mbox::split(&bytes);
    drop(bytes);
    let mut done = Done { size: t.size, ..Done::default() };
    if messages.is_empty() {
        return Err(("unreadable", done));
    }
    for m in messages {
        let target = m.folder.as_deref().or(t.folder.as_deref()).filter(|f| !f.trim().is_empty()).unwrap_or("inbox");
        let Some(folder) = folder_for(&mb, &mut known, target).await else {
            done.skipped += 1;
            continue;
        };
        let date = m.internal_date.filter(|d| (0..=4_102_444_800).contains(d)).unwrap_or(now);
        match mb.append_dated(folder, bytes::Bytes::from(m.raw), if m.flags.bits() <= 31 { m.flags } else { Flags::default() }, date).await {
            Ok(_) => done.messages += 1,
            // A mailbox that is full takes no more: what went in stays, and the transfer says why it stopped.
            Err(Error::Quota(_)) => return Err(("quota", done)),
            Err(_) => done.skipped += 1,
        }
    }
    Ok(done)
}

#[async_trait]
impl Service for TransferService {
    fn name(&self) -> &'static str {
        "mailbox-transfer"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let mut last_sweep = tokio::time::Instant::now();
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match TransferService::run_once(&ctx).await {
                // One after another without waiting while there is work.
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "a mailbox transfer could not be run"),
            }
            if last_sweep.elapsed() > Duration::from_secs(300) {
                last_sweep = tokio::time::Instant::now();
                if let Err(e) = ctx.transfers.fail_stuck(STUCK_AFTER_SECS).await {
                    tracing::warn!(error = %e, "stuck mailbox transfers could not be swept");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(self.poll) => {}
            }
        }
    }
}
