//! `GcRepo`: the backend-neutral garbage-collection repository (PG5).
//!
//! The [`Gc`](crate::Gc) facade keeps everything that is not a database concern: the grace
//! window and its minimum, the blob-file walk, file deletion (strictly AFTER the row commit),
//! quarantine healing, the error report and the batch loops. The repository answers in plain
//! data and owns its transactions.
//!
//! # The `zero_since` invariant
//!
//! Postgres: a BEFORE trigger assigns `NEW.zero_since` (the database clock when the refcount
//! reaches 0, NULL while referenced) and `CHECK ((refcount = 0) = (zero_since IS NOT NULL))`
//! makes the invariant a constraint (migration `0001`, hardened in `f41fef8`; killing tests in
//! `pg_schema_cases.rs`, behaviour on both backends in `gc_conformance.rs`). SQLite keeps its
//! AFTER triggers: `ALTER TABLE .. ADD CHECK` does not exist, a table rebuild would have to
//! re-create every foreign key onto `blobs`, and the existing GC tests deliberately build
//! corrupt rows (`refcount > 0` with an old `zero_since`) to prove GC still refuses to delete
//! them. The observable behaviour is the same; the strengthening is Postgres-only by decision.
//!
//! # Clock
//!
//! The row cutoff is `db_now() - grace` (decision D2): the clock that stamped
//! `blobs.zero_since`. File cutoffs are `min(db_cutoff, fs_now - grace)` and a run is refused
//! when the database and volume clocks differ by more than a quarter of the grace window
//! (see the facade, clock policy rule 4). `collect_blobs_at` takes an explicit time for tests only.
//!
//! # Locking
//!
//! * `delete_blob_rows` touches ONLY `blobs`. On Postgres it locks the candidate rows with
//!   `lock_existing_blobs` (ascending hash order, `FOR NO KEY UPDATE`) BEFORE the delete, then
//!   deletes the rows that are still `refcount = 0 AND zero_since < cutoff`. Every other path
//!   that changes a refcount locks blob rows in the same order (mailbox row first, then blobs
//!   ascending), so a GC batch cannot form a cycle with an append, `purge_deleted` or
//!   `delete_folder`. A concurrent append that wants the same blob either locked it first (the
//!   delete then sees `refcount > 0` and skips the row) or waits for the delete, finds the row
//!   gone and re-inserts it (`lock_blobs` loop). The file is removed by the facade only after
//!   the commit and only when its mtime is older than the cutoff, so an append that refreshed
//!   the file before inserting its row keeps it.
//! * `purge_deleted_batch` removes soft-deleted messages across mailboxes by delegating, per
//!   mailbox, to `MailboxRepo::purge_deleted` (mailbox row first, blobs ascending): no second
//!   copy of the locking rule exists.
//! * The delivery and capability sweeps delete by key in small batches (`SKIP LOCKED`), never
//!   touch blobs and wait for no lock.
//! * The singleton rule (one GC per blob root) stays the blob-root file lock taken by the
//!   facade; moving it into the database is a separate spec item and must use a
//!   transaction-scoped lock (`pg_try_advisory_xact_lock`), never a session lock (D6). Batch
//!   correctness does not depend on the singleton: concurrent batches lock in the same order.

use async_trait::async_trait;
use rusty_core::{BlobHash, Result};

#[async_trait]
pub trait GcRepo: Send + Sync {
    /// The database clock in unix seconds (a read).
    async fn db_now(&self) -> Result<i64>;
    /// Hashes of unreferenced rows (`refcount = 0 AND zero_since < cutoff_secs`) greater than
    /// `after`, ascending, at most `limit` (a read).
    async fn blob_candidates(
        &self,
        cutoff_secs: i64,
        after: Option<&BlobHash>,
        limit: i64,
    ) -> Result<Vec<BlobHash>>;
    /// Delete, in one transaction, those of `hashes` that are still unreferenced and past the
    /// cutoff; returns the hashes whose rows were deleted. Any database error rolls the whole
    /// batch back (`Err`); the caller isolates a bad row by retrying one hash at a time.
    async fn delete_blob_rows(&self, hashes: &[BlobHash], cutoff_secs: i64) -> Result<Vec<BlobHash>>;
    /// All row hashes whose first byte is `shard` (a read).
    async fn blob_rows_in_shard(&self, shard: u8) -> Result<Vec<String>>;
    /// Whether a row exists for `hash` (a read).
    async fn blob_row_exists(&self, hash: &BlobHash) -> Result<bool>;
    /// Hard-delete up to about `limit` soft-deleted messages with `deleted_at < older_than`
    /// (those kept by a `deliveries` row excepted); returns how many. 0 means none are left.
    async fn purge_deleted_batch(&self, older_than: i64, limit: i64) -> Result<u64>;
    /// Delete up to `limit` `deliveries` rows with `created_at < older_than`.
    async fn purge_deliveries_batch(&self, older_than: i64, limit: i64) -> Result<u64>;
    /// Delete up to `limit` capabilities that expired or were revoked before `older_than`.
    async fn purge_capabilities_batch(&self, older_than: i64, limit: i64) -> Result<u64>;
}
