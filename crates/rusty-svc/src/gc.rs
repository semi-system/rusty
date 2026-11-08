//! The GC ticker: purges old delivery ids and dead capabilities and collects unreferenced blobs,
//! all through the `Gc` facade of the `Context` (never the backend-specific free functions).

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::service::Service;

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcSummary {
    pub deliveries_purged: u64,
    pub capabilities_purged: u64,
    pub blobs_deleted: u64,
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
        let grace = Duration::from_secs(ctx.cfg.gc_grace_seconds);
        match ctx.gc.collect_blobs(&*ctx.blobs, grace).await {
            Ok(r) => {
                s.blobs_deleted = r.files_deleted;
                s.errors += r.errors.len();
            }
            // Another process holds the GC lock: that pass is theirs.
            Err(Error::Conflict(_)) => {}
            Err(e) => step_failed(&mut s, "collect blobs", &e),
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
