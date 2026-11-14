//! Transaction helper of the Postgres repositories: bounded retry on 40P01 / 40001, mailbox
//! row lock first, commit/rollback classification, retry statistics and the test knobs.
//!
//! Transaction discipline (READ COMMITTED):
//! 1. `tx_retry!` owns BEGIN / COMMIT and the retry loop. A unit of work is a block that is
//!    re-run from scratch when Postgres reports a deadlock (40P01) or serialization failure
//!    (40001); after [`MAX_ATTEMPTS`] the error becomes `Error::Busy` (transient, D1).
//! 2. For mailbox writes the first statement is `SELECT .. FROM mailboxes WHERE id = $1 FOR
//!    UPDATE` (`tx_retry!(self, Some(mb), ..)`), so writers of one mailbox are serialized and
//!    writers of different mailboxes run in parallel. The lock statement runs INSIDE the retry
//!    loop: a deadlock while waiting for it is retried like any other.
//! 3. `blobs` rows are locked in ASCENDING hash order (`lock_blobs`) BEFORE any statement that
//!    fires the refcount trigger, so two mailboxes storing overlapping blob sets cannot
//!    deadlock on `blobs`. The retry loop is the safety net, not the mechanism.
//! 4. Errors: statements return `sqlx::Error` through `TxError::Db` so the loop can look at the
//!    SQLSTATE; everything else is mapped by `map_db_error` (SQLSTATE only, never message text).
//! 5. NEVER retried: connection errors and a failed COMMIT with unknown outcome (the commit may
//!    have happened); `deliver` is safe to repeat by the caller because of its `DeliveryId`.
//!
//! The body of `tx_retry!` may run several times: it must have no side effects outside the
//! transaction (blob IO happens before the repository call). Post-commit hooks (cache
//! notification) run after the macro, never inside the loop; every write returns `Committed` with
//! the last change seq for them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use rusty_core::{Error, MailboxId};
use sqlx::PgConnection;

use super::PgRepo;
use crate::repo::not_found_mailbox;
use crate::error_map::busy;
use crate::map_db_error;

/// Attempts of one unit of work (first try included).
pub const MAX_ATTEMPTS: u32 = 5;

/// Error inside a unit of work: a database error (kept raw so its SQLSTATE can be inspected by
/// the retry loop) or an already-final domain error.
#[derive(Debug)]
pub enum TxError {
    Db(sqlx::Error),
    Domain(Error),
}

impl From<sqlx::Error> for TxError {
    fn from(e: sqlx::Error) -> Self {
        TxError::Db(e)
    }
}

impl From<Error> for TxError {
    fn from(e: Error) -> Self {
        TxError::Domain(e)
    }
}

pub type TxResult<T> = std::result::Result<T, TxError>;

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(|d| d.code().map(|c| c.into_owned()))
}

/// 40P01 deadlock_detected, 40001 serialization_failure: the whole transaction may be re-run.
pub fn is_retryable(e: &sqlx::Error) -> bool {
    matches!(sqlstate(e).as_deref(), Some("40P01") | Some("40001"))
}

fn is_deadlock(e: &sqlx::Error) -> bool {
    sqlstate(e).as_deref() == Some("40P01")
}

/// Counters of the retry loop (process-lifetime, per `PgRepo`).
#[derive(Debug, Default)]
pub struct PgRetryStats {
    pub retries: AtomicU64,
    pub deadlocks: AtomicU64,
    pub exhausted: AtomicU64,
}

/// Test-only fault injection: lets a test force the interleaving that the ordering rule
/// prevents. Compiled ONLY with the `test-knobs` cargo feature, which only this crate's own
/// tests enable (self dev-dependency; `scripts/check-raw-db.sh` and the release symbol check of
/// `scripts/check-test-knobs.sh` enforce that). Without the feature this type is zero-sized
/// and none of the paths it controls exist in the binary.
#[cfg(not(feature = "test-knobs"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct Knobs;

/// Test-only fault injection: lets a test force the interleaving that the ordering rule
/// prevents. Never set in production code paths (all fields are off by default).
#[cfg(feature = "test-knobs")]
#[derive(Debug, Clone, Copy, Default)]
pub struct Knobs {
    /// Lock blobs one by one in the order the caller supplies them instead of ascending.
    pub unordered_blob_locks: bool,
    /// `pg_sleep` this long after the first blob lock (only with `unordered_blob_locks`).
    pub hold_after_first_blob_ms: u32,
    /// With `unordered_blob_locks`: iterate the supplied blobs in REVERSE (a crossed order).
    pub reverse_blob_lock_order: bool,
    /// Keep the transaction open this long after the work and before COMMIT (two-session
    /// interleavings without sleeps in the statements).
    pub pause_before_commit_ms: u32,
    /// Do not wait in the in-process per-mailbox queue (two writers of one mailbox race for the row lock).
    pub bypass_mailbox_queue: bool,
    /// Omit the mailbox row lock (exercises the second line of defence: triggers, FKs, counters).
    pub skip_mailbox_lock: bool,
    /// `lock_blobs` deletes the blob rows it has just inserted, every round (a GC that always wins
    /// the race): exercises the bounded retry loop giving up.
    pub drop_blob_rows_after_insert: bool,
    /// `pg_sleep` this long between the blob INSERT and the blob LOCK of `lock_blobs`: the window
    /// in which a concurrent GC can delete an unreferenced row the INSERT skipped (DO NOTHING).
    pub pause_after_blob_insert_ms: u32,
}

pub(super) enum Step<T> {
    Done(T),
    Retry,
    Fail(Error),
}

impl PgRepo {
    /// BEGIN (READ COMMITTED). Pool exhaustion / a closed pool are `Busy` (see
    /// `PgStore::begin_write`).
    pub(super) async fn begin_tx(
        &self,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, Error> {
        self.store.begin_write().await
    }

    /// Take this mailbox's turn in the in-process write queue (see `pg_queue.rs`); `None` for operations
    /// that lock no mailbox. The test knob `bypass_mailbox_queue` skips it so a test can race two
    /// writers of one mailbox on purpose.
    pub(super) async fn take_turn(
        &self,
        mb: Option<MailboxId>,
    ) -> Result<Option<crate::MailboxPermit>, Error> {
        let Some(mb) = mb else { return Ok(None) };
        #[cfg(feature = "test-knobs")]
        if self.knobs.bypass_mailbox_queue || self.knobs.skip_mailbox_lock {
            return Ok(None);
        }
        self.store.mailbox_turn(mb).await.map(Some)
    }

    /// Lock the mailbox row FIRST (`SELECT .. FOR UPDATE`). `NotFound` if it does not exist.
    pub(super) async fn lock_mailbox(
        &self,
        c: &mut PgConnection,
        mb: MailboxId,
    ) -> TxResult<()> {
        sqlx::query_scalar::<_, i64>(crate::repo::pg_lit("SELECT id FROM mailboxes WHERE id = $1 FOR UPDATE"))
            .bind(mb.0)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(not_found_mailbox)?;
        Ok(())
    }

    /// Test knob: raise a real 40P01 inside the transaction while the counter is positive.
    #[cfg(feature = "test-knobs")]
    pub(super) async fn maybe_inject_deadlock(&self, c: &mut PgConnection) -> TxResult<()> {
        let mut armed = false;
        let mut cur = self.inject_deadlocks.load(Ordering::SeqCst);
        while cur > 0 {
            match self.inject_deadlocks.compare_exchange(
                cur,
                cur - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    armed = true;
                    break;
                }
                Err(seen) => cur = seen,
            }
        }
        if armed {
            sqlx::query(
                crate::repo::pg_lit("DO $$ BEGIN RAISE EXCEPTION 'injected' USING ERRCODE = '40P01'; END $$"),
            )
            .execute(&mut *c)
            .await?;
        }
        Ok(())
    }

    /// Test knob: tell a waiting test that this session has run its unit of work and still holds
    /// every lock of its open transaction (the pause before COMMIT follows), so a second session
    /// can be started without guessing with a sleep.
    #[cfg(feature = "test-knobs")]
    pub(super) fn signal_work_done(&self) {
        if let Some(n) = &self.work_done {
            n.notify_one();
        }
    }

    /// Finish one attempt: commit on success; roll back and classify on failure.
    pub(super) async fn finish<T>(
        &self,
        tx: sqlx::Transaction<'static, sqlx::Postgres>,
        res: TxResult<T>,
        attempt: u32,
    ) -> Step<T> {
        let failure = match res {
            Ok(v) => match tx.commit().await {
                Ok(()) => return Step::Done(v),
                Err(e) => TxError::Db(e),
            },
            Err(e) => {
                // The transaction may already be aborted; the rollback result is moot.
                let _ = tx.rollback().await;
                e
            }
        };
        match failure {
            TxError::Db(e) if is_retryable(&e) => {
                self.stats.retries.fetch_add(1, Ordering::Relaxed);
                if is_deadlock(&e) {
                    self.stats.deadlocks.fetch_add(1, Ordering::Relaxed);
                }
                #[cfg(feature = "mutation-testing")]
                let max_attempts = if crate::mutation::flag("no-retry") { 1 } else { MAX_ATTEMPTS };
                #[cfg(not(feature = "mutation-testing"))]
                let max_attempts = MAX_ATTEMPTS;
                if attempt + 1 >= max_attempts {
                    self.stats.exhausted.fetch_add(1, Ordering::Relaxed);
                    return Step::Fail(busy("transaction conflict"));
                }
                backoff(attempt).await;
                Step::Retry
            }
            TxError::Db(e) => Step::Fail(map_db_error(e)),
            TxError::Domain(e) => Step::Fail(e),
        }
    }
}

/// Run `$body` (a block evaluating to `TxResult<T>`, with `$c: &mut PgConnection` in scope) in
/// a READ COMMITTED transaction, optionally locking the mailbox row first, retrying the WHOLE
/// block on 40P01/40001 up to `MAX_ATTEMPTS` times. See the module docs for the rules.
///
/// A macro instead of a closure-taking method: a closure `for<'c> FnMut(&'c mut PgConnection)
/// -> Pin<Box<dyn Future + 'c>>` cannot capture borrowed arguments of the calling
/// `async_trait` method (rustc: "returning this value requires that `'life1` must outlive
/// `'static`"). Inlining the loop keeps every borrow in one future.
macro_rules! tx_retry {
    ($self:expr, $lock:expr, |$c:ident| $body:block) => {{
        let lock_id: Option<rusty_core::MailboxId> = $lock;
        // The per-mailbox turn comes BEFORE the connection: a hot mailbox's writers wait here, not
        // in the pool (pg_queue.rs). Held across every retry of this operation.
        let _turn = $self.take_turn(lock_id).await?;
        let mut attempt = 0u32;
        loop {
            let mut tx = $self.begin_tx().await?;
            let res: $crate::repo::pg::tx::TxResult<_> = async {
                let $c: &mut sqlx::PgConnection = &mut *tx;
                let lock: Option<rusty_core::MailboxId> = lock_id;
                if let Some(mb) = lock {
                    #[cfg(feature = "test-knobs")]
                    let skip = $self.knobs.skip_mailbox_lock;
                    #[cfg(not(feature = "test-knobs"))]
                    let skip = false;
                    if !skip {
                        $self.lock_mailbox(&mut *$c, mb).await?;
                    }
                }
                #[cfg(feature = "test-knobs")]
                $self.maybe_inject_deadlock(&mut *$c).await?;
                $body
            }
            .await;
            #[cfg(feature = "test-knobs")]
            {
                $self.signal_work_done();
                if res.is_ok() && $self.knobs.pause_before_commit_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(u64::from(
                        $self.knobs.pause_before_commit_ms,
                    )))
                    .await;
                }
            }
            match $self.finish(tx, res, attempt).await {
                $crate::repo::pg::tx::Step::Done(v) => break Ok::<_, rusty_core::Error>(v),
                $crate::repo::pg::tx::Step::Retry => attempt += 1,
                $crate::repo::pg::tx::Step::Fail(e) => break Err(e),
            }
        }
    }};
}
pub(crate) use tx_retry;

/// 5 ms * 2^attempt plus up to the same again of jitter.
async fn backoff(attempt: u32) {
    let base = 5u64 << attempt.min(6);
    let jitter = u64::from(OsRng.next_u32()) % (base + 1);
    tokio::time::sleep(Duration::from_millis(base + jitter)).await;
}

/// Production builds carry no knob state.
#[cfg(not(feature = "test-knobs"))]
const _: () = assert!(std::mem::size_of::<Knobs>() == 0);
