//! The store upkeep ticker: every blob store gets its turn at `BlobStore::maintain` (a pool makes missing copies, carries out the deletions a backend missed and empties a draining
//! backend). It is separate from garbage collection because it has to run often (a failover pool is only as safe as its second copy is recent), while collection runs rarely.

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::Result;
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::service::Service;

/// How many objects of each kind one store may handle per pass.
const BATCH: u32 = 200;

pub struct MaintainService {
    every: Duration,
}

impl MaintainService {
    pub fn new(every: Duration) -> MaintainService {
        MaintainService { every }
    }

    /// One pass over every store; the number of things done. A store that cannot be opened (not ready, unreachable) is skipped and logged; the others still get their turn.
    pub async fn run_once(ctx: &Context) -> Result<u64> {
        let mut done = 0u64;
        for id in ctx.gc.store_ids().await? {
            let store = match ctx.stores.store(id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(store = id.0, error = %e, "blob store skipped by upkeep");
                    continue;
                }
            };
            match store.maintain(BATCH).await {
                Ok(n) => done += n,
                Err(e) => tracing::warn!(store = id.0, error = %e, "blob store upkeep failed"),
            }
        }
        Ok(done)
    }
}

#[async_trait]
impl Service for MaintainService {
    fn name(&self) -> &'static str {
        "store-upkeep"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let mut tick = tokio::time::interval(self.every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    tokio::select! {
                        () = shutdown.cancelled() => return Ok(()),
                        r = MaintainService::run_once(&ctx) => match r {
                            Ok(n) if n > 0 => tracing::info!(done = n, "blob store upkeep"),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "blob store upkeep pass failed"),
                        },
                    }
                }
            }
        }
    }
}
