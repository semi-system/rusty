//! The worker of the moves of a mailbox from another IMAP server (docs/design-imap-migration.md). It runs where the mailboxes are (the IMAP service): it takes the oldest queued migration, opens the
//! password the administrator sealed for it, reads the other server's folders and messages (`imap_pull`), appends each message to the account's mailbox with its date and flags, and reports its progress
//! in the database where the admin web reads it. It never changes anything on the other server, and every failure is a fixed word.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Flags, FolderId, Result};
use rusty_store::{Mailbox, Migration, Progress, MIGRATION_SECRET_OWNER, MIGRATION_SECRET_PURPOSE};
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::imap_pull::{self, PullError, Remote, RemoteFolder, Security, Session, MAX_MESSAGE};
use crate::service::Service;

/// A migration `running` this long without a sign of life was left by a worker that stopped.
const STUCK_AFTER_SECS: i64 = 900;
/// The worker reports its progress every this many messages (and at every folder).
const REPORT_EVERY: i64 = 20;

pub struct MigrationService {
    poll: Duration,
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl MigrationService {
    pub fn new() -> MigrationService {
        MigrationService { poll: Duration::from_secs(3), tls: None }
    }

    pub fn with_poll(mut self, poll: Duration) -> MigrationService {
        self.poll = poll;
        self
    }

    /// The TLS configuration for the other servers (the host's trust store when none is given).
    pub fn with_tls(mut self, tls: Arc<rustls::ClientConfig>) -> MigrationService {
        self.tls = Some(tls);
        self
    }

    /// Run the oldest queued migration, if any, to its end; whether there was one.
    pub async fn run_once(&self, ctx: &Context) -> Result<bool> {
        let Some(m) = ctx.migrations.claim().await? else { return Ok(false) };
        let mut p = Progress::default();
        let outcome = match self.pull(ctx, &m, &mut p).await {
            Ok(Outcome::Done) => ("done", None),
            Ok(Outcome::Cancelled) => ("cancelled", None),
            Err(word) => ("failed", Some(word)),
        };
        ctx.migrations.finish(m.id, outcome.0, outcome.1, &p).await?;
        Ok(true)
    }

    async fn pull(&self, ctx: &Context, m: &Migration, p: &mut Progress) -> std::result::Result<Outcome, &'static str> {
        let password = password_of(ctx, m).await?;
        let security = Security::parse(&m.security).ok_or("failed")?;
        let remote = Remote { host: m.host.clone(), port: u16::try_from(m.port).map_err(|_| "failed")?, security, user: m.remote_user.clone(), password };
        let tls = match &self.tls {
            Some(t) => t.clone(),
            None => imap_pull::system_tls().map_err(PullError::word)?,
        };
        let mb = ctx.hub.open_for(AccountId(m.account)).await.map_err(|e| word_of(&e))?;
        let mut session = Session::connect(&remote, &tls).await.map_err(PullError::word)?;
        let result = copy_all(ctx, m, &mut session, &mb, p).await;
        session.logout().await;
        result
    }
}

impl Default for MigrationService {
    fn default() -> MigrationService {
        MigrationService::new()
    }
}

enum Outcome {
    Done,
    Cancelled,
}

fn word_of(e: &Error) -> &'static str {
    match e {
        Error::Quota(_) => "quota",
        _ => "failed",
    }
}

/// The password of the other server: the sealed secret of this migration, opened with the key this service holds.
async fn password_of(ctx: &Context, m: &Migration) -> std::result::Result<String, &'static str> {
    let unsealer = ctx.unsealer.as_ref().ok_or("failed")?;
    let row = ctx.sealed.get(MIGRATION_SECRET_OWNER, m.id, MIGRATION_SECRET_PURPOSE).await.map_err(|_| "failed")?.ok_or("failed")?;
    let context = rusty_seal::Context::new(MIGRATION_SECRET_OWNER, m.id, MIGRATION_SECRET_PURPOSE).map_err(|_| "failed")?;
    let plain = unsealer.unseal(&context, &row.sealed).map_err(|_| "failed")?;
    String::from_utf8(plain.to_vec()).map_err(|_| "failed")
}

/// The local folder for a folder of the other server: the one of the same role (the Sent folder of the other server into the Sent folder here), else one with its name.
async fn local_folder(mb: &Mailbox, known: &mut Vec<(String, FolderId)>, f: &RemoteFolder) -> Option<FolderId> {
    if let Some(role) = f.role {
        if let Some((_, id)) = known.iter().find(|(n, _)| n.eq_ignore_ascii_case(role)) {
            return Some(*id);
        }
    }
    // A name here has no `/`: the other server's hierarchy is flattened with dots.
    let name = f.name.replace('/', ".");
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

async fn copy_all(ctx: &Context, m: &Migration, s: &mut Session, mb: &Mailbox, p: &mut Progress) -> std::result::Result<Outcome, &'static str> {
    let remote_folders = s.folders().await.map_err(PullError::word)?;
    p.folders_total = i32::try_from(remote_folders.len()).unwrap_or(i32::MAX);
    let local = mb.list_folders().await.map_err(|e| word_of(&e))?;
    let mut known: Vec<(String, FolderId)> = local.iter().flat_map(|f| [(f.name.clone(), f.id), (f.role.clone(), f.id)]).collect();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    for rf in &remote_folders {
        p.folder = Some(rf.name.clone());
        if !ctx.migrations.progress(m.id, p).await.map_err(|e| word_of(&e))? {
            return Ok(Outcome::Cancelled);
        }
        let Some(target) = local_folder(mb, &mut known, rf).await else {
            p.folders_done += 1;
            continue;
        };
        // A folder that cannot be opened is passed over; the rest goes on.
        if s.examine(&rf.name).await.is_err() {
            p.folders_done += 1;
            continue;
        }
        for uid in s.uids().await.map_err(PullError::word)? {
            let got = match s.fetch(uid).await {
                Ok(Some(g)) => g,
                Ok(None) => continue,
                // The server refused this one message (it was removed meanwhile): counted, and the rest goes on. A broken connection or a time-out is the end.
                Err(PullError::Protocol) => {
                    p.skipped += 1;
                    continue;
                }
                Err(e) => return Err(e.word()),
            };
            if got.flags.contains(Flags::DELETED) || got.body.is_empty() || got.body.len() > MAX_MESSAGE {
                p.skipped += 1;
                continue;
            }
            let size = i64::try_from(got.body.len()).unwrap_or(0);
            let flags = Flags::from_bits(got.flags.bits() & !Flags::DELETED.bits()).unwrap_or_default();
            let date = got.date.filter(|d| (0..=4_102_444_800).contains(d)).unwrap_or(now);
            match mb.append_dated(target, bytes::Bytes::from(got.body), flags, date).await {
                Ok(_) => {
                    p.messages += 1;
                    p.bytes += size;
                }
                // A mailbox that is full takes no more: what went in stays, and the migration says why it stopped.
                Err(Error::Quota(_)) => return Err("quota"),
                Err(_) => p.skipped += 1,
            }
            if (p.messages + p.skipped) % REPORT_EVERY == 0 && !ctx.migrations.progress(m.id, p).await.map_err(|e| word_of(&e))? {
                return Ok(Outcome::Cancelled);
            }
        }
        p.folders_done += 1;
    }
    Ok(Outcome::Done)
}

#[async_trait]
impl Service for MigrationService {
    fn name(&self) -> &'static str {
        "imap-migration"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let mut last_sweep = tokio::time::Instant::now();
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match self.run_once(&ctx).await {
                // One after another without waiting while there is work.
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "a migration could not be run"),
            }
            if last_sweep.elapsed() > Duration::from_secs(300) {
                last_sweep = tokio::time::Instant::now();
                if let Err(e) = ctx.migrations.fail_stuck(STUCK_AFTER_SECS).await {
                    tracing::warn!(error = %e, "stuck migrations could not be swept");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(self.poll) => {}
            }
        }
    }
}
