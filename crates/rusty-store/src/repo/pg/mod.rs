//! Postgres implementation of the repositories (directory: `directory.rs`; mailbox:
//! `mailbox.rs`). Transaction helper, retry loop and test knobs: `tx.rs`.

mod directory;
mod mailbox;
pub mod tx;

use std::collections::BTreeMap;
#[cfg(feature = "test-knobs")]
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use sqlx::PgConnection;

use super::{hash_bytes, NewMessage};
use crate::PgStore;
use tx::TxResult;

pub use tx::{is_retryable, Knobs, PgRetryStats, MAX_ATTEMPTS};

#[derive(Clone)]
pub struct PgRepo {
    store: PgStore,
    stats: Arc<PgRetryStats>,
    /// Zero-sized without the `test-knobs` feature.
    #[cfg_attr(not(feature = "test-knobs"), allow(dead_code))]
    knobs: Knobs,
    /// Test knob: number of upcoming attempts that raise a real 40P01.
    #[cfg(feature = "test-knobs")]
    inject_deadlocks: Arc<AtomicU32>,
    /// Test knob: notified each time a session finished its work with its transaction (and locks) still open.
    #[cfg(feature = "test-knobs")]
    work_done: Option<Arc<tokio::sync::Notify>>,
}

impl PgRepo {
    #[allow(clippy::default_constructed_unit_structs)] // a unit struct without `test-knobs`
    pub fn new(store: PgStore) -> Self {
        Self {
            store,
            stats: Arc::default(),
            knobs: Knobs::default(),
            #[cfg(feature = "test-knobs")]
            inject_deadlocks: Arc::default(),
            #[cfg(feature = "test-knobs")]
            work_done: None,
        }
    }

    #[cfg(feature = "test-knobs")]
    #[doc(hidden)]
    pub fn with_knobs_for_test(mut self, knobs: Knobs) -> Self {
        self.knobs = knobs;
        self
    }

    /// Test knob: `signal.notify_one()` is called right after a session of this repository has
    /// finished its work, transaction and locks still open (a stored permit if nobody waits yet), so a test starts the
    /// second session when the first one provably holds its lock instead of sleeping.
    #[cfg(feature = "test-knobs")]
    #[doc(hidden)]
    pub fn with_work_done_signal_for_test(mut self, signal: Arc<tokio::sync::Notify>) -> Self {
        self.work_done = Some(signal);
        self
    }

    /// Test knob: the next `n` transaction attempts of `tx_retry!` operations fail with a real
    /// 40P01 raised by the server.
    #[cfg(feature = "test-knobs")]
    #[doc(hidden)]
    pub fn inject_deadlocks_for_test(&self, n: u32) {
        self.inject_deadlocks.store(n, Ordering::SeqCst);
    }

    pub fn stats(&self) -> &PgRetryStats {
        &self.stats
    }
}

/// Lock blob rows that EXIST (deletes: the rows are referenced by messages this transaction is
/// about to delete) with `FOR NO KEY UPDATE`, in ASCENDING hash order, before the first
/// statement that fires the refcount trigger. `hashes` may be in any order and contain
/// duplicates; the ordering is established here. Missing rows are ignored.
#[cfg_attr(not(feature = "test-knobs"), allow(unused_variables))]
async fn lock_existing_blobs(
    c: &mut PgConnection,
    hashes: &[Vec<u8>],
    knobs: Knobs,
) -> TxResult<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    #[cfg(feature = "test-knobs")]
    {
    if knobs.unordered_blob_locks {
        // Fault-injection path: the caller's order (or its reverse), one lock per statement,
        // optional pause after the first.
        let mut seen = std::collections::HashSet::new();
        let mut order: Vec<&Vec<u8>> = hashes.iter().filter(|h| seen.insert((*h).clone())).collect();
        if knobs.reverse_blob_lock_order {
            order.reverse();
        }
        let mut first = true;
        for h in order {
            sqlx::query("SELECT hash FROM blobs WHERE hash = $1 FOR NO KEY UPDATE")
                .bind(h)
                .fetch_all(&mut *c)
                .await?;
            if first && knobs.hold_after_first_blob_ms > 0 {
                sqlx::query("SELECT pg_sleep($1)")
                    .bind(f64::from(knobs.hold_after_first_blob_ms) / 1000.0)
                    .execute(&mut *c)
                    .await?;
            }
            first = false;
        }
        return Ok(());
    }
    }
    let mut asc: Vec<Vec<u8>> = hashes.to_vec();
    asc.sort_unstable();
    asc.dedup();
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT hash FROM blobs WHERE hash = ANY($1) ORDER BY hash FOR NO KEY UPDATE",
    )
    .bind(&asc)
    .fetch_all(&mut *c)
    .await?;
    Ok(())
}

/// Insert missing blob rows and lock ALL of them (`FOR NO KEY UPDATE`) in ascending hash order.
///
/// After this call the refcount trigger of any message insert finds the rows already locked by
/// this transaction, so no further `blobs` lock is ever acquired out of order. Loops (bounded)
/// if a concurrent GC deleted an unreferenced row between the insert and the lock.
#[cfg_attr(not(feature = "test-knobs"), allow(unused_variables))]
async fn lock_blobs(
    c: &mut PgConnection,
    msgs: &[&NewMessage],
    knobs: Knobs,
) -> TxResult<()> {
    // BTreeMap over raw bytes: ascending byte order == bytea order in Postgres.
    let mut blobs: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
    for m in msgs {
        blobs.entry(hash_bytes(&m.hash)?).or_insert(m.size);
    }
    #[cfg(feature = "test-knobs")]
    {
    if knobs.unordered_blob_locks {
        // Fault-injection path: input order (reversed with `reverse_blob_lock_order`), one lock
        // per statement, optional pause.
        let mut order: Vec<&&NewMessage> = msgs.iter().collect();
        if knobs.reverse_blob_lock_order {
            order.reverse();
        }
        let mut first = true;
        for m in order {
            let h = hash_bytes(&m.hash)?;
            sqlx::query("INSERT INTO blobs (hash, size) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING")
                .bind(&h)
                .bind(m.size)
                .execute(&mut *c)
                .await?;
            sqlx::query("SELECT hash FROM blobs WHERE hash = $1 FOR NO KEY UPDATE")
                .bind(&h)
                .fetch_all(&mut *c)
                .await?;
            if first && knobs.hold_after_first_blob_ms > 0 {
                sqlx::query("SELECT pg_sleep($1)")
                    .bind(f64::from(knobs.hold_after_first_blob_ms) / 1000.0)
                    .execute(&mut *c)
                    .await?;
            }
            first = false;
        }
        return Ok(());
    }
    }
    let hashes: Vec<Vec<u8>> = blobs.keys().cloned().collect();
    let sizes: Vec<i64> = blobs.values().copied().collect();
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO blobs (hash, size) \
             SELECT t.h, t.s FROM unnest($1::bytea[], $2::bigint[]) AS t(h, s) ORDER BY t.h \
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&hashes)
        .bind(&sizes)
        .execute(&mut *c)
        .await?;
        #[cfg(feature = "test-knobs")]
        if knobs.drop_blob_rows_after_insert {
            sqlx::query("DELETE FROM blobs WHERE hash = ANY($1)")
                .bind(&hashes)
                .execute(&mut *c)
                .await?;
        }
        let locked: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT hash FROM blobs WHERE hash = ANY($1) ORDER BY hash FOR NO KEY UPDATE",
        )
        .bind(&hashes)
        .fetch_all(&mut *c)
        .await?;
        if locked.len() == hashes.len() {
            return Ok(());
        }
    }
    // A GC that keeps deleting the row between insert and lock is transient contention, not a
    // storage fault: Busy is retryable by the caller.
    Err(crate::sqlite::busy("blob row contention").into())
}
