//! Garbage collection: hard-delete expunged messages, then remove unreferenced blobs (spec §5).
//!
//! [`Gc`] is a facade over [`GcRepo`](crate::repo::GcRepo) (SQLite and Postgres). The free
//! functions below are the SQLite entry points kept for existing callers.
//!
//! Safety argument for `collect_blobs`:
//! - A `blobs` row is deleted only inside a transaction that re-checks `refcount = 0` and the
//!   age of `zero_since` after taking the row lock (Postgres: `lock_existing_blobs`, ascending
//!   hash order; SQLite: the global write lock); `messages.blob_hash` is an `ON DELETE
//!   RESTRICT` foreign key, so even a corrupted refcount cannot drop a referenced row.
//! - A blob file is deleted only after its row is gone (or when it never had one), only AFTER
//!   the row deletion committed, and only if its mtime is older than the grace window.
//!   `Mailbox::append` writes (or, for existing content, re-touches) the file *before* its
//!   transaction inserts the row, so a file that is about to gain a row is always younger than
//!   the grace window, provided the window is longer than the slowest `put` -> commit of an
//!   append. Use hours, not seconds ([`DEFAULT_GC_GRACE`]); [`Gc::collect_blobs`] refuses less
//!   than [`MIN_GC_GRACE`].
//! - At most one GC runs per blob root: `collect_blobs` takes the store's advisory lock
//!   (`<blob root>/.gc.lock`, `File::try_lock`) and fails fast with `Conflict` otherwise. This
//!   is a file lock, not a database session lock (nothing in the database outlives a
//!   transaction, so the repositories are safe behind a transaction-pooling proxy).
//! - File deletion renames the blob aside first (`.<hash>.gc<pid>.<n>.gcq`, see
//!   `BlobStore::delete_if_older`). A referenced blob can therefore read `NotFound` from
//!   `BlobStore::get` only in one narrow race (GC renamed it aside just as a `put` of the same
//!   content refreshed it); it is moved back within microseconds, and callers do not retry. A
//!   quarantine file left by a crash is healed at the start of the next run
//!   (`BlobStore::heal_quarantine`): restored if the blob path is missing, removed if the blob
//!   path exists and the copy is past the grace window.
//!
//! Clock (decision D2): the cutoff is the DATABASE clock minus the grace, the clock that
//! stamped `blobs.zero_since`. Only the `_at` twins take a caller time, for tests.
//!
//! Memory: rows are read in batches of [`GC_BATCH`]; files are walked one shard (1/256 of the
//! store) at a time, together with the row hashes of that shard. The error list is capped at
//! [`MAX_GC_ERRORS`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusty_core::{BlobHash, Error, Result};

use crate::blob::unix_secs;
use crate::repo::{sqlite::SqliteRepo, GcRepo};
use crate::{BlobStore, SqliteStore};

/// Messages hard-deleted per transaction by [`purge_deleted`].
pub const PURGE_BATCH: i64 = 1000;
/// `blobs` rows examined (and deleted) per transaction by [`collect_blobs`].
pub const GC_BATCH: i64 = 1000;
/// At most this many messages are kept in [`GcReport::errors`].
pub const MAX_GC_ERRORS: usize = 1000;
/// A conservative grace window for production use.
pub const DEFAULT_GC_GRACE: Duration = Duration::from_secs(24 * 3600);
/// The shortest grace window [`collect_blobs`] accepts.
pub const MIN_GC_GRACE: Duration = Duration::from_secs(3600);
/// How long a delivery id is remembered by default.
pub const DELIVERY_RETENTION_SECS: i64 = 7 * 86400;
/// How long a dead (expired or revoked) capability row is kept by default.
pub const CAPABILITY_RETENTION_SECS: i64 = 86400;

/// Outcome of one [`collect_blobs`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// `blobs` rows deleted.
    pub rows_deleted: u64,
    /// Blob files deleted (formerly referenced blobs and orphan files).
    pub files_deleted: u64,
    /// Stale temporary files deleted.
    pub tmp_swept: u64,
    /// Quarantined blobs moved back to their blob path at startup.
    pub quarantine_restored: u64,
    /// Stale quarantine copies removed at startup.
    pub quarantine_removed: u64,
    /// Per-blob failures that were skipped. Messages name blob hashes or shards only: no
    /// content and no filesystem paths.
    pub errors: Vec<String>,
    /// Failures beyond [`MAX_GC_ERRORS`] that were counted but not recorded.
    pub errors_dropped: u64,
}

impl GcReport {
    fn error(&mut self, what: impl FnOnce() -> String) {
        if self.errors.len() < MAX_GC_ERRORS {
            self.errors.push(what());
        } else {
            self.errors_dropped += 1;
        }
    }
}

/// `Display` of a domain error plus its direct source (io / database errors carry no paths).
fn describe(e: &Error) -> String {
    match std::error::Error::source(e) {
        Some(src) => format!("{e}: {src}"),
        None => e.to_string(),
    }
}

/// The garbage collector over a [`GcRepo`].
#[derive(Clone)]
pub struct Gc {
    repo: Arc<dyn GcRepo>,
}

impl Gc {
    pub fn new(repo: Arc<dyn GcRepo>) -> Gc {
        Gc {
            repo,
        }
    }

    /// Hard-delete soft-deleted messages with `deleted_at < older_than` (unix seconds), in
    /// batches of [`PURGE_BATCH`], until none are left. Keywords cascade, blob refcounts follow
    /// via triggers, `change_log` is untouched. A message that still has a `deliveries` row is
    /// kept (so a replayed delivery id stays a `Duplicate` and never resurrects an expunged
    /// message); it is purged by a later run once [`purge_deliveries`](Self::purge_deliveries)
    /// has removed the row (<= [`DELIVERY_RETENTION_SECS`]). Returns how many messages were
    /// removed.
    pub async fn purge_deleted(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let n = self.repo.purge_deleted_batch(older_than, PURGE_BATCH).await?;
            if n == 0 {
                return Ok(total);
            }
            total += n;
        }
    }

    /// Delete `deliveries` rows with `created_at < older_than` (unix seconds), in batches.
    /// Messages are untouched; a retry arriving after its row is gone creates a new message,
    /// so `older_than` must leave the whole retry window ([`DELIVERY_RETENTION_SECS`]) intact.
    pub async fn purge_deliveries(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let n = self.repo.purge_deliveries_batch(older_than, PURGE_BATCH).await?;
            total += n;
            if n < PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }

    /// [`purge_deliveries`](Self::purge_deliveries) for rows older than `retention_secs`
    /// before `now` (unix seconds).
    #[doc(hidden)]
    pub async fn purge_deliveries_at(&self, now: i64, retention_secs: i64) -> Result<u64> {
        self.purge_deliveries(now.saturating_sub(retention_secs.max(0))).await
    }

    /// [`purge_deliveries_at`](Self::purge_deliveries_at) with the database clock and
    /// [`DELIVERY_RETENTION_SECS`].
    pub async fn purge_deliveries_default(&self) -> Result<u64> {
        let now = self.repo.db_now().await?;
        self.purge_deliveries_at(now, DELIVERY_RETENTION_SECS).await
    }

    /// Delete `capabilities` rows that are dead before `older_than` (unix seconds): `expires_at
    /// < older_than` or `revoked_at < older_than`, in batches. A live row (unexpired,
    /// unrevoked) is never touched while `older_than` is not in the future.
    pub async fn purge_capabilities(&self, older_than: i64) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let n = self.repo.purge_capabilities_batch(older_than, PURGE_BATCH).await?;
            total += n;
            if n < PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }

    /// [`purge_capabilities`](Self::purge_capabilities) for rows dead for more than
    /// `retention_secs` before `now`.
    #[doc(hidden)]
    pub async fn purge_capabilities_at(&self, now: i64, retention_secs: i64) -> Result<u64> {
        self.purge_capabilities(now.saturating_sub(retention_secs.max(0))).await
    }

    /// [`purge_capabilities_at`](Self::purge_capabilities_at) with the database clock and
    /// [`CAPABILITY_RETENTION_SECS`].
    pub async fn purge_capabilities_default(&self) -> Result<u64> {
        let now = self.repo.db_now().await?;
        self.purge_capabilities_at(now, CAPABILITY_RETENTION_SECS).await
    }

    /// Collect unreferenced blobs now (phases: see [`collect_blobs_at`](Self::collect_blobs_at)).
    /// A `grace` below [`MIN_GC_GRACE`] is `Invalid`: it must exceed the slowest `put` ->
    /// commit of an append.
    pub async fn collect_blobs(&self, blobs: &dyn BlobStore, grace: Duration) -> Result<GcReport> {
        if grace < MIN_GC_GRACE {
            return Err(Error::Invalid("gc grace must be at least one hour".into()));
        }
        // The grace cutoff comes from the DATABASE clock (clock policy D2), the same clock that
        // stamped `blobs.zero_since`. Blob file mtimes are compared against the same cutoff; the
        // minimum grace of one hour bounds the effect of any skew between the database and the
        // filesystem clock.
        let db_now = self.repo.db_now().await?;
        let now = UNIX_EPOCH + Duration::from_secs(u64::try_from(db_now).unwrap_or(0));
        self.collect_blobs_at(blobs, grace, now).await
    }

    /// Remove unreferenced blobs as seen at time `now`, with any `grace` (for tests; production
    /// code calls [`collect_blobs`](Self::collect_blobs)). Under the GC lock (`Conflict` if
    /// another GC holds it):
    /// 0. heal quarantine files left by an interrupted run;
    /// 1. rows with `refcount = 0` and `zero_since < now - grace` (seconds): the rows are
    ///    deleted in a transaction that re-checks both conditions, then the files are deleted
    ///    if their mtime is older than `now - grace` (a younger file is left as an orphan for a
    ///    later run);
    /// 2. files without a row whose mtime is older than `now - grace` (the row's absence is
    ///    re-checked right before the delete);
    /// 3. temporary files older than `now - grace`.
    ///
    /// A blob with `refcount > 0` is never deleted. Errors on one blob are recorded in
    /// [`GcReport::errors`] and the run continues; `Err` only when the GC lock is held
    /// elsewhere (`Conflict`), the lock cannot be taken, or the database cannot be queried at
    /// all.
    #[doc(hidden)]
    pub async fn collect_blobs_at(
        &self,
        blobs: &dyn BlobStore,
        grace: Duration,
        now: SystemTime,
    ) -> Result<GcReport> {
        let cutoff = now.checked_sub(grace).unwrap_or(UNIX_EPOCH);
        let cutoff_secs = unix_secs(cutoff);
        // Held until this function returns (or unwinds).
        let _lock = blobs.try_lock_gc().await?;
        let mut report = GcReport::default();
        match blobs.heal_quarantine(cutoff).await {
            Ok(h) => {
                report.quarantine_restored = h.restored;
                report.quarantine_removed = h.removed;
                if h.failed > 0 {
                    report.error(|| format!("quarantine: {} files could not be healed", h.failed));
                }
            }
            Err(e) => report.error(|| format!("quarantine: {}", describe(&e))),
        }
        self.collect_rows(blobs, cutoff, cutoff_secs, &mut report).await?;
        self.collect_orphans(blobs, cutoff, cutoff_secs, &mut report).await;
        match blobs.sweep_temp_before(cutoff).await {
            Ok(n) => report.tmp_swept = n,
            Err(e) => report.error(|| format!("temporary files: {}", describe(&e))),
        }
        Ok(report)
    }

    /// Phase 1: unreferenced rows past the grace window, then their files.
    async fn collect_rows(
        &self,
        blobs: &dyn BlobStore,
        cutoff: SystemTime,
        cutoff_secs: i64,
        report: &mut GcReport,
    ) -> Result<()> {
        let mut after: Option<BlobHash> = None;
        loop {
            let batch = self
                .repo
                .blob_candidates(cutoff_secs, after.as_ref(), GC_BATCH)
                .await?;
            let Some(last) = batch.last() else {
                return Ok(());
            };
            after = Some(last.clone());
            let full = batch.len() as i64 >= GC_BATCH;

            let deleted = match self.repo.delete_blob_rows(&batch, cutoff_secs).await {
                Ok(d) => d,
                Err(Error::Busy(_)) => {
                    report.error(|| format!("blob rows from {}: database busy", batch[0].as_str()));
                    Vec::new()
                }
                // One bad row (e.g. the RESTRICT foreign key of a still-referenced blob) rolled
                // the batch back: isolate it by deleting one hash at a time.
                Err(_) if batch.len() > 1 => {
                    let mut ok = Vec::new();
                    for h in &batch {
                        match self.repo.delete_blob_rows(std::slice::from_ref(h), cutoff_secs).await {
                            Ok(mut d) => ok.append(&mut d),
                            Err(e) => report.error(|| {
                                format!("blob {}: delete row: {}", h.as_str(), describe(&e))
                            }),
                        }
                    }
                    ok
                }
                Err(e) => {
                    report.error(|| {
                        format!("blob {}: delete row: {}", batch[0].as_str(), describe(&e))
                    });
                    Vec::new()
                }
            };

            report.rows_deleted += deleted.len() as u64;
            // Files go only AFTER the row deletion committed.
            for hash in deleted {
                match blobs.delete_if_older(&hash, cutoff).await {
                    Ok(true) => report.files_deleted += 1,
                    Ok(false) => {}
                    Err(e) => report
                        .error(|| format!("blob {}: delete file: {}", hash.as_str(), describe(&e))),
                }
            }
            if !full {
                return Ok(());
            }
        }
    }

    /// Phase 2: files that have no `blobs` row, one shard at a time.
    async fn collect_orphans(
        &self,
        blobs: &dyn BlobStore,
        cutoff: SystemTime,
        cutoff_secs: i64,
        report: &mut GcReport,
    ) {
        for shard in 0..=u8::MAX {
            let entries = match blobs.list_shard(shard).await {
                Ok(v) => v,
                Err(e) => {
                    report.error(|| format!("shard {shard:02x}: list: {}", describe(&e)));
                    continue;
                }
            };
            // mtime < cutoff implies floor(mtime) <= floor(cutoff); delete_if_older decides exactly.
            let old: Vec<BlobHash> = entries
                .into_iter()
                .filter(|e| e.modified_unix <= cutoff_secs)
                .map(|e| e.hash)
                .collect();
            if old.is_empty() {
                continue;
            }
            let rows: HashSet<String> = match self.repo.blob_rows_in_shard(shard).await {
                Ok(v) => v.into_iter().collect(),
                Err(e) => {
                    report.error(|| format!("shard {shard:02x}: read rows: {}", describe(&e)));
                    continue;
                }
            };
            for hash in old.into_iter().filter(|h| !rows.contains(h.as_str())) {
                // Re-check right before deleting: an append may have committed a row since.
                match self.repo.blob_row_exists(&hash).await {
                    Ok(false) => {}
                    Ok(true) => continue,
                    Err(e) => {
                        report.error(|| {
                            format!("blob {}: check row: {}", hash.as_str(), describe(&e))
                        });
                        continue;
                    }
                }
                match blobs.delete_if_older(&hash, cutoff).await {
                    Ok(true) => report.files_deleted += 1,
                    Ok(false) => {}
                    Err(e) => report.error(|| {
                        format!("blob {}: delete orphan file: {}", hash.as_str(), describe(&e))
                    }),
                }
            }
        }
    }
}

// ---------------------------------------------------------------- SQLite entry points

fn sqlite_gc(store: &SqliteStore) -> Gc {
    Gc::new(Arc::new(SqliteRepo::new(store.clone())))
}

/// [`Gc::purge_deleted`] on a SQLite store.
pub async fn purge_deleted(store: &SqliteStore, older_than: i64) -> Result<u64> {
    sqlite_gc(store).purge_deleted(older_than).await
}

/// [`Gc::purge_deliveries`] on a SQLite store.
pub async fn purge_deliveries(store: &SqliteStore, older_than: i64) -> Result<u64> {
    sqlite_gc(store).purge_deliveries(older_than).await
}

/// [`Gc::purge_deliveries_at`] on a SQLite store.
#[doc(hidden)]
pub async fn purge_deliveries_at(
    store: &SqliteStore,
    now: i64,
    retention_secs: i64,
) -> Result<u64> {
    sqlite_gc(store).purge_deliveries_at(now, retention_secs).await
}

/// [`Gc::purge_deliveries_default`] on a SQLite store.
pub async fn purge_deliveries_default(store: &SqliteStore) -> Result<u64> {
    sqlite_gc(store).purge_deliveries_default().await
}

/// [`Gc::purge_capabilities`] on a SQLite store.
pub async fn purge_capabilities(store: &SqliteStore, older_than: i64) -> Result<u64> {
    sqlite_gc(store).purge_capabilities(older_than).await
}

/// [`Gc::purge_capabilities_at`] on a SQLite store.
#[doc(hidden)]
pub async fn purge_capabilities_at(
    store: &SqliteStore,
    now: i64,
    retention_secs: i64,
) -> Result<u64> {
    sqlite_gc(store).purge_capabilities_at(now, retention_secs).await
}

/// [`Gc::purge_capabilities_default`] on a SQLite store.
pub async fn purge_capabilities_default(store: &SqliteStore) -> Result<u64> {
    sqlite_gc(store).purge_capabilities_default().await
}

/// [`Gc::collect_blobs`] on a SQLite store.
pub async fn collect_blobs(
    store: &SqliteStore,
    blobs: &dyn BlobStore,
    grace: Duration,
) -> Result<GcReport> {
    sqlite_gc(store).collect_blobs(blobs, grace).await
}

/// [`collect_blobs`] reading "now" from `clock_sql` (a statement returning one integer, unix
/// seconds) instead of the real database clock. Tests pass a skewed clock to prove that the
/// cutoff follows the DATABASE and not the process clock.
#[doc(hidden)]
pub async fn collect_blobs_with_db_clock(
    store: &SqliteStore,
    blobs: &dyn BlobStore,
    grace: Duration,
    clock_sql: &str,
) -> Result<GcReport> {
    if grace < MIN_GC_GRACE {
        return Err(Error::Invalid("gc grace must be at least one hour".into()));
    }
    let db_now: i64 = sqlx::query_scalar(clock_sql)
        .fetch_one(store.pool())
        .await
        .map_err(crate::map_db_error)?;
    let now = UNIX_EPOCH + Duration::from_secs(u64::try_from(db_now).unwrap_or(0));
    sqlite_gc(store).collect_blobs_at(blobs, grace, now).await
}

/// [`Gc::collect_blobs_at`] on a SQLite store.
#[doc(hidden)]
pub async fn collect_blobs_at(
    store: &SqliteStore,
    blobs: &dyn BlobStore,
    grace: Duration,
    now: SystemTime,
) -> Result<GcReport> {
    sqlite_gc(store).collect_blobs_at(blobs, grace, now).await
}
