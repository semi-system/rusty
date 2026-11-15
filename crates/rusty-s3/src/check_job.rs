//! The `storage.check` job: "can this store be used?", answered by the service that holds the unsealing key (docs/design-s3-per-tenant.md, section 7).
//!
//! The admin web cannot read the store's keys back, so it cannot test the bucket itself. It enqueues this job; the mail service opens the store from its settings and sealed keys exactly as
//! it will for real mail, runs [`probe`](crate::probe::probe) and stores the **fixed category** of the result with its time on the store's row. No server text, host or key reaches the
//! database or the job's error. A store is used only after a check recorded `ok`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{DomainId, Result};
use rusty_store::repo::{BlobStoreRow, StoreId};
use rusty_store::BlobStoreAdmin;
use rusty_svc::{Context, Service};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::opener::S3StoreOpener;
use crate::probe::probe;

/// The job kind.
pub use rusty_core::storage_policy::STORAGE_CHECK_JOB_KIND;

/// Ask for a check of the store `store` of `domain`. The result appears on the store row (`rustyctl storage show`, the admin web).
pub async fn enqueue_storage_check(jobs: &rusty_store::JobQueue, domain: DomainId, store: StoreId) -> Result<()> {
    jobs.enqueue(STORAGE_CHECK_JOB_KIND, Some(domain), &json!({ "store": store.0 }), None).await?;
    Ok(())
}

/// What runs a check: the category of its result (`ok`, `config` for a store that cannot even be opened, or one of the failure names).
#[async_trait]
pub trait Prober: Send + Sync {
    async fn probe(&self, row: &BlobStoreRow, nonce: u64) -> &'static str;
}

/// The real check: open the client of the row (settings re-validated, keys unsealed, addresses policed) and probe it.
pub struct S3Prober {
    opener: Arc<S3StoreOpener>,
}

impl S3Prober {
    pub fn new(opener: Arc<S3StoreOpener>) -> S3Prober {
        S3Prober { opener }
    }
}

#[async_trait]
impl Prober for S3Prober {
    async fn probe(&self, row: &BlobStoreRow, nonce: u64) -> &'static str {
        let client = match self.opener.client(row).await {
            Ok(c) => c,
            Err(_) => return "config",
        };
        match probe(&client, nonce).await {
            Ok(()) => "ok",
            Err(f) => f.name(),
        }
    }
}

pub struct StorageCheckWorker {
    stores: BlobStoreAdmin,
    prober: Arc<dyn Prober>,
    poll: Duration,
}

impl StorageCheckWorker {
    pub fn new(stores: BlobStoreAdmin, prober: Arc<dyn Prober>) -> StorageCheckWorker {
        StorageCheckWorker { stores, prober, poll: Duration::from_secs(2) }
    }

    /// How long an idle worker sleeps between queue polls.
    pub fn with_poll(mut self, poll: Duration) -> StorageCheckWorker {
        self.poll = poll;
        self
    }

    /// Run the check of one store and record its category. `Ok(())` also for a store that is gone or is not an S3 store (nothing to record).
    async fn check(&self, store: StoreId, nonce: u64) -> Result<()> {
        let Some(row) = self.stores.get(store).await? else { return Ok(()) };
        if row.kind != "s3" {
            return Ok(());
        }
        let category = self.prober.probe(&row, nonce).await;
        self.stores.record_check(store, category).await
    }
}

fn store_of(payload: &Value) -> Option<StoreId> {
    payload.get("store").and_then(Value::as_i64).filter(|s| *s > 1).map(StoreId)
}

#[async_trait]
impl Service for StorageCheckWorker {
    fn name(&self) -> &'static str {
        "storage-check"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        tracing::info!("storage check worker started");
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match ctx.jobs.claim("storage-check", &[STORAGE_CHECK_JOB_KIND], 300).await {
                Ok(Some(job)) => {
                    let outcome = match store_of(&job.payload) {
                        Some(store) => self.check(store, (job.id.0 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ job.epoch as u64).await,
                        // Not something we wrote: nothing to check, nothing to retry.
                        None => Ok(()),
                    };
                    let settled = match outcome {
                        Ok(()) => ctx.jobs.complete(job.id, job.epoch).await,
                        // The result could not be recorded: the job goes back in the queue (a fixed text; no detail of the error is kept).
                        Err(_) => ctx.jobs.fail(job.id, job.epoch, "the result could not be recorded", 30).await,
                    };
                    if let Err(e) = settled {
                        tracing::warn!(job = job.id.0, error = %e, "storage check not settled; it will be retried");
                    }
                }
                Ok(None) => {
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.poll) => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "storage check queue claim failed");
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.poll.max(Duration::from_secs(1))) => {}
                    }
                }
            }
        }
        tracing::info!("storage check worker stopped");
        Ok(())
    }
}
