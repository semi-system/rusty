//! U5 spike 1: can the blob GC collect a queued message body while its job still retries?
//!
//! Finding: YES. `JobOutbound` stores the body as a blob and puts only its hash in the job payload;
//! the blob has no `refcount` row, so it is an orphan file for the GC, which removes it once its
//! mtime is older than the grace window. With the old 1-day default grace a message retried for 5
//! days lost its body on day 2. Decision (documented in the plan): keep the body in the blob store
//! and make the configuration refuse a grace window shorter than `outbound.max_age_secs` + 1 day
//! (`StartupConfig`, tested in rusty-core), instead of hiding the body in an "Outbox" folder (which
//! would drag a hidden system folder through IMAP, quota and mailbox deletion). If a later
//! feature needs the body to outlive any grace, the Outbox folder is the fallback.
mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use common::*;
use rusty_core::config::StartupConfig;
use rusty_store::{BlobStore, FsBlobStore, Gc};

const DAY: u64 = 86_400;

async fn gc_run(grace_secs: u64, age_days: u64) -> bool {
    let b = pg_backend().await.expect("a Postgres test server (RUSTY_TEST_PG_URL)");
    let root = b.tmp.path().join("blobs");
    FsBlobStore::init(&root).await.unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&root).await.unwrap());
    let hash = blobs.put(Bytes::from_static(b"Subject: queued\r\n\r\nbody\r\n")).await.unwrap();
    let gc = Gc::new(b.store.repos().gc.clone());
    let now = SystemTime::now() + Duration::from_secs(age_days * DAY);
    gc.collect_blobs_at(blobs.as_ref(), Duration::from_secs(grace_secs), now).await.unwrap();
    blobs.get(&hash).await.is_ok()
}

// backend-neutral: blob store and GC only, any backend gives the reference-free orphan rule
#[tokio::test]
async fn an_unreferenced_queued_body_is_collected_after_the_grace_window() {
    // The hazard, with the pre-U5 default of one day: day 2 of a 5-day retry the body is gone.
    assert!(!gc_run(DAY, 2).await, "the old default grace window would lose queued mail");
}

// backend-neutral: blob store and GC only, any backend gives the reference-free orphan rule
#[tokio::test]
async fn with_the_validated_grace_the_body_survives_the_whole_retry_window() {
    let cfg = StartupConfig::from_toml_str("database_path = \"/a/db\"\nblob_dir = \"/a/b\"\n").unwrap();
    assert!(cfg.gc_grace_seconds >= cfg.outbound.max_age_secs + DAY);
    // Day 5: the last retry and the DSN still find the body (the DSN quotes the original headers).
    assert!(gc_run(cfg.gc_grace_seconds, 5).await);
}
