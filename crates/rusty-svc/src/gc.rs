//! The GC ticker: purges old delivery ids and dead capabilities and collects unreferenced blobs,
//! all through the `Gc` facade of the `Context` (never the backend-specific free functions).

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::service::Service;

/// How long a row of the delivery log is kept.
const LOG_RETENTION_SECS: i64 = 30 * 86_400;
const AUDIT_RETENTION_SECS: i64 = 365 * 86_400;

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcSummary {
    pub deliveries_purged: u64,
    pub capabilities_purged: u64,
    pub blobs_deleted: u64,
    /// Closed accounts removed with their mailboxes, and the messages of those mailboxes.
    pub accounts_removed: u64,
    pub account_messages_removed: u64,
    /// Rows of the delivery log past their retention.
    pub log_purged: u64,
    /// Rows of the audit log past their 365 days.
    pub audit_purged: u64,
    /// Rows of the vacation book-keeping older than 60 days.
    pub vacation_purged: u64,
    /// Exports and imports of mailboxes older than a day, removed with their pieces.
    pub transfers_purged: u64,
    /// Rows of the per-day statistics written and removed.
    pub stats_rolled: u64,
    pub stats_purged: u64,
    /// Messages people deleted that were kept for recovery and are now removed for good (their bodies are freed by the blob collection below).
    pub deleted_purged: u64,
    /// Steps that failed (logged); a failed step never stops the others.
    pub errors: usize,
}

pub struct GcService {
    every: Duration,
}

impl GcService {
    /// Run a pass every `every`; the first one runs after the first interval.
    pub fn new(every: Duration) -> GcService {
        GcService { every }
    }

    /// One pass.
    pub async fn run_once(ctx: &Context) -> Result<GcSummary> {
        let mut s = GcSummary::default();
        match ctx.gc.purge_deliveries_default().await {
            Ok(n) => s.deliveries_purged = n,
            Err(e) => step_failed(&mut s, "purge deliveries", &e),
        }
        match ctx.gc.purge_capabilities_default().await {
            Ok(n) => s.capabilities_purged = n,
            Err(e) => step_failed(&mut s, "purge capabilities", &e),
        }
        // Closed accounts past their retention, with their mail (docs/design-delete.md); the blobs they held are collected below once nothing refers to them.
        match ctx.gc.purge_closed_accounts_default(i64::from(ctx.cfg.closed_account_days) * 86_400).await {
            Ok((messages, accounts)) => {
                s.account_messages_removed = messages;
                s.accounts_removed = accounts;
            }
            Err(e) => step_failed(&mut s, "purge closed accounts", &e),
        }
        // Deleted (expunged) messages are kept for `deleted_retention_days` so that an administrator can recover one; after that they and their bodies go.
        match ctx.jobs.now().await {
            Ok(now) => match ctx.gc.purge_deleted(now.saturating_sub(i64::from(ctx.cfg.deleted_retention_days) * 86_400)).await {
                Ok(n) => s.deleted_purged = n,
                Err(e) => step_failed(&mut s, "purge deleted messages", &e),
            },
            Err(e) => step_failed(&mut s, "read the clock", &e),
        }
        // The days the log holds are counted into the statistics before the log is cut, and the statistics are kept 400 days.
        match ctx.jobs.now().await {
            Ok(now) => {
                match ctx.queue.roll_up_stats(now).await {
                    Ok(n) => s.stats_rolled = n,
                    Err(e) => step_failed(&mut s, "roll up the statistics", &e),
                }
                match ctx.queue.purge_stats(now.div_euclid(86_400) - 400, 5000).await {
                    Ok(n) => s.stats_purged = n,
                    Err(e) => step_failed(&mut s, "purge the statistics", &e),
                }
            }
            Err(e) => step_failed(&mut s, "read the clock", &e),
        }
        // The delivery log is kept for 30 days (docs/design-queue.md).
        match ctx.jobs.now().await {
            Ok(now) => match ctx.queue.purge_log(now.saturating_sub(LOG_RETENTION_SECS), 5000).await {
                Ok(n) => s.log_purged = n,
                Err(e) => step_failed(&mut s, "purge the delivery log", &e),
            },
            Err(e) => step_failed(&mut s, "read the clock", &e),
        }
        // The audit log of the admin web is kept for 365 days (docs/design-audit.md).
        match ctx.jobs.now().await {
            Ok(now) => match ctx.audit.purge(now.saturating_sub(AUDIT_RETENTION_SECS), 5000).await {
                Ok(n) => s.audit_purged = n,
                Err(e) => step_failed(&mut s, "purge the audit log", &e),
            },
            Err(e) => step_failed(&mut s, "read the clock", &e),
        }
        match ctx.jobs.now().await {
            Ok(now) => match ctx.rules.purge_vacation_replies(now.saturating_sub(60 * 86_400), 5000).await {
                Ok(n) => s.vacation_purged = n,
                Err(e) => step_failed(&mut s, "purge the vacation replies", &e),
            },
            Err(e) => step_failed(&mut s, "read the clock", &e),
        }
        match ctx.transfers.purge_expired(1000).await {
            Ok(n) => s.transfers_purged = n,
            Err(e) => step_failed(&mut s, "purge the mailbox transfers", &e),
        }
        let grace = Duration::from_secs(ctx.cfg.gc_grace_seconds);
        // One collection per blob store: each store's rows and objects are its own.
        match ctx.gc.store_ids().await {
            Ok(ids) => {
                for id in ids {
                    let store = match ctx.stores.store(id).await {
                        Ok(st) => st,
                        // A store that cannot be opened (its settings, its disk, its bucket) is skipped and counted; the others are still collected.
                        Err(e) => {
                            step_failed(&mut s, "open blob store", &e);
                            continue;
                        }
                    };
                    match ctx.gc.for_store(id).collect_blobs(&*store, grace).await {
                        Ok(r) => {
                            s.blobs_deleted += r.files_deleted;
                            s.errors += r.errors.len();
                        }
                        // Another process holds this store's GC lock: that pass is theirs.
                        Err(Error::Conflict(_)) => {}
                        Err(e) => step_failed(&mut s, "collect blobs", &e),
                    }
                }
            }
            Err(e) => step_failed(&mut s, "list blob stores", &e),
        }
        Ok(s)
    }
}

fn step_failed(s: &mut GcSummary, step: &str, e: &Error) {
    s.errors += 1;
    tracing::warn!(step, error = %e, "gc step failed");
}

#[async_trait]
impl Service for GcService {
    fn name(&self) -> &'static str {
        "gc"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let mut tick = tokio::time::interval(self.every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick completes at once
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    let pass = GcService::run_once(&ctx);
                    tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        r = pass => match r {
                            Ok(s) => tracing::debug!(?s, "gc pass"),
                            Err(e) => tracing::warn!(error = %e, "gc pass failed"),
                        },
                    }
                }
            }
        }
    }
}
