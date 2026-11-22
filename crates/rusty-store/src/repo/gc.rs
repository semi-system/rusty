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
//! `pg_schema_cases.rs`, behaviour in `gc_conformance.rs`). A referenced blob with an old `zero_since` therefore cannot
//! exist, and GC still re-checks the refcount under the row lock.
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
//! * The singleton rule (one GC per blob root) is the blob-root file lock (`flock`) taken by the
//!   facade. It is sound only for processes that share ONE local volume on ONE kernel; it is
//!   NOT sound on NFS without a working lock daemon, nor across hosts. The database side does
//!   not depend on it: concurrent batches lock in the same order (mailbox row, blob rows
//!   ascending, job row), and each volume's orphan phase only touches its own files. A
//!   database singleton cannot be a transaction-scoped advisory lock, because one GC run spans
//!   many transactions, and must not be a session lock (D6, PgBouncer). The planned multi-host
//!   design is a lease row with an epoch (the `jobs` table already has that mechanism); see the
//!   PG5 carry-over in the Postgres plan (PG8) and the distributed-model spec.

use async_trait::async_trait;
use rusty_core::{BlobHash, MailboxId, Result};

use super::blob_store::StoreId;

/// Outcome of one [`GcRepo::purge_deleted_batch`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeBatch {
    /// Messages hard-deleted.
    pub purged: u64,
    /// Mailboxes skipped because they were busy (their lock timed out); retry on a later run.
    pub busy_mailboxes: Vec<MailboxId>,
}

/// Outcome of one [`GcRepo::purge_closed_account_batch`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClosedAccountBatch {
    /// Messages hard-deleted from the mailbox of the account this call worked on.
    pub messages: u64,
    /// Accounts removed (the last batch of an account removes it together with its mailbox).
    pub accounts: u64,
    /// Whether a closed account was due at all: `false` means there is nothing to do.
    pub worked: bool,
}

#[async_trait]
pub trait GcRepo: Send + Sync {
    /// The database clock in unix seconds (a read).
    async fn db_now(&self) -> Result<i64>;
    /// Hashes of unreferenced rows (`refcount = 0 AND zero_since < cutoff_secs`) greater than
    /// `after`, ascending, at most `limit` (a read).
    async fn blob_candidates(
        &self,
        store: StoreId,
        cutoff_secs: i64,
        after: Option<&BlobHash>,
        limit: i64,
    ) -> Result<Vec<BlobHash>>;
    /// Delete, in one transaction, those of `hashes` that are still unreferenced and past the
    /// cutoff; returns the hashes whose rows were deleted. Any database error rolls the whole
    /// batch back (`Err`); the caller isolates a bad row by retrying one hash at a time.
    async fn delete_blob_rows(&self, store: StoreId, hashes: &[BlobHash], cutoff_secs: i64) -> Result<Vec<BlobHash>>;
    /// All row hashes whose first byte is `shard` (a read).
    async fn blob_rows_in_shard(&self, store: StoreId, shard: u8) -> Result<Vec<String>>;
    /// Like `blob_rows_in_shard`, but only the rows that something still references (`refcount > 0`): the blobs a restore must find (a read).
    async fn blob_rows_in_use_in_shard(&self, store: StoreId, shard: u8) -> Result<Vec<String>>;
    /// Whether a row exists for `hash` (a read).
    async fn blob_row_exists(&self, store: StoreId, hash: &BlobHash) -> Result<bool>;
    /// Every blob store that exists (a read), the platform store first: garbage collection runs once for each.
    async fn blob_store_ids(&self) -> Result<Vec<StoreId>>;
    /// Take (or renew, for the same `holder`) the collection lease of a store for `ttl_secs` of the database clock: `Ok(true)` when this holder has it, `Ok(false)` when another holder
    /// does and it has not expired. The lease is what keeps two collectors, in this process or in others, off one store (the in-process and file locks of the stores cannot).
    async fn acquire_gc_lease(&self, store: StoreId, holder: &str, ttl_secs: i64) -> Result<bool>;
    /// Give the lease back (a no-op for a lease somebody else holds).
    async fn release_gc_lease(&self, store: StoreId, holder: &str) -> Result<()>;
    /// Hard-delete up to about `limit` soft-deleted messages per mailbox with `deleted_at <
    /// older_than` (those kept by a `deliveries` row excepted), across several mailboxes.
    /// `purged == 0` means none are left that could be purged. A mailbox that is `Busy` (lock
    /// timeout) is skipped and reported in `busy_mailboxes`; it never stops the batch (review
    /// PG5 M-e). Any other error aborts the batch.
    async fn purge_deleted_batch(&self, older_than: i64, limit: i64) -> Result<PurgeBatch>;
    /// Delete up to `limit` `deliveries` rows with `created_at < older_than`.
    async fn purge_deliveries_batch(&self, older_than: i64, limit: i64) -> Result<u64>;
    /// Delete up to `limit` capabilities that expired or were revoked before `older_than`.
    async fn purge_capabilities_batch(&self, older_than: i64, limit: i64) -> Result<u64>;
    /// Work on ONE closed account whose `closed_at` is before `older_than`: delete up to `limit` messages of its mailbox, and when none are left delete the account (its mailbox, folders,
    /// aliases and capabilities go with it by cascade, the blobs it held are collected by the blob pass). Messages go in small transactions so no statement runs long. `worked: false`: none is due.
    async fn purge_closed_account_batch(&self, older_than: i64, limit: i64) -> Result<ClosedAccountBatch>;
}
