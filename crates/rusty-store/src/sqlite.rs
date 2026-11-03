use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use rusty_core::{Error, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Sqlite, SqlitePool, Transaction};

/// sqlx's default, stated explicitly: how long a writer waits for the SQLite write lock.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// SQLite-backed metadata store. Every pooled connection has
/// `PRAGMA foreign_keys = ON`; `open` verifies it.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (and migrate) a database. `sqlite::memory:` yields a private
    /// in-memory database served by a single connection so that all queries
    /// see the same data.
    ///
    /// A database file that does not exist yet is created empty with mode 0600 before sqlx
    /// opens it (SQLite gives its journal/WAL files the database file's mode). An existing
    /// file keeps its mode. The directory holding it should be 0700 and owned by the service
    /// account (run with umask 077; see SECURITY.md).
    pub async fn open(url: &str) -> Result<SqliteStore> {
        Self::open_with_busy_timeout(url, DEFAULT_BUSY_TIMEOUT).await
    }

    /// [`open`](Self::open) with an explicit `busy_timeout`: how long a connection waits for
    /// the database write lock before the operation fails with [`Error::Busy`].
    pub async fn open_with_busy_timeout(
        url: &str,
        busy_timeout: Duration,
    ) -> Result<SqliteStore> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| Error::storage_with("invalid database url", e))?
            .foreign_keys(true)
            .busy_timeout(busy_timeout)
            .create_if_missing(true);
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        if !in_memory {
            precreate_private(opts.get_filename())?;
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(if in_memory { 1 } else { 4 })
            .min_connections(if in_memory { 1 } else { 0 })
            .idle_timeout(None)
            .max_lifetime(None)
            // sqlx closes an idle connection if `acquire` is cancelled during its liveness
            // ping; for the single in-memory connection that would silently swap in a new,
            // empty database. An in-process database needs no ping.
            .test_before_acquire(!in_memory)
            .connect_with(opts)
            .await
            .map_err(|e| Error::storage_with("cannot open database", e))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| Error::storage_with("migration failed", e))?;
        let store = SqliteStore { pool };
        store.verify_foreign_keys().await?;
        Ok(store)
    }

    /// The underlying connection pool.
    ///
    /// Internal: public only because `rusty-directory` and tests issue queries directly. It
    /// will be removed once the store service owns all database access; do not build new
    /// callers on it.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Begin a write transaction with `BEGIN IMMEDIATE`, taking SQLite's write lock up
    /// front so concurrent writers serialize instead of failing on lock upgrade.
    ///
    /// Internal: public only for `rusty-directory`, GC and tests; it will be removed once the
    /// store service owns all database access.
    ///
    /// Dropping the returned transaction without committing (an early `?` return or a
    /// cancelled future) queues a rollback on the connection's worker, which runs before the
    /// connection is reused, so the write lock is never leaked.
    ///
    /// The begin step itself runs in its own task: in sqlx 0.8.6, cancelling `begin_with`
    /// after SQLite acknowledged the `BEGIN` but before sqlx's follow-up check returns leaves
    /// an open transaction (holding the write lock) on a pooled connection. In a separate
    /// task the begin always completes; if our caller is gone, the finished transaction is
    /// dropped there and rolled back. Covered by `cancelled_begin_write_does_not_leak_transaction`.
    ///
    /// Requires a Tokio runtime (it calls `tokio::spawn`); calling it outside one panics.
    pub async fn begin_write(&self) -> Result<Transaction<'static, Sqlite>> {
        let pool = self.pool.clone();
        tokio::spawn(async move { pool.begin_with("BEGIN IMMEDIATE").await })
            .await
            .map_err(|e| Error::storage_with("begin task failed", e))?
            .map_err(map_db_error)
    }

    async fn verify_foreign_keys(&self) -> Result<()> {
        let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::storage_with("cannot read PRAGMA foreign_keys", e))?;
        if on != 1 {
            return Err(Error::storage("PRAGMA foreign_keys is not enabled"));
        }
        Ok(())
    }
}

/// Create `path` as an empty file with mode 0600 if it does not exist (an empty file is a
/// valid, empty SQLite database). Losing a creation race to another process is fine.
fn precreate_private(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow_cloexec_flags())
        .open(path);
    match res {
        Ok(f) => f
            .sync_all()
            .map_err(|e| Error::storage_with("cannot create database file", e)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(Error::storage_with("cannot create database file", e)),
    }
}

/// `O_NOFOLLOW | O_CLOEXEC` as raw open flags (via rustix, no libc dependency).
fn nofollow_cloexec_flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32
}

/// Map a database error to a domain error without leaking raw SQLite text.
/// UNIQUE/PRIMARY KEY -> `Conflict("already exists")`, FK -> `Conflict("referenced row
/// missing or still in use")`, CHECK/NOT NULL -> `Invalid("invalid value")`. Trigger
/// `RAISE(ABORT, ..)` messages (ours) pass through: a `conflict:` prefix gives `Conflict`,
/// anything else `Invalid`. Everything else is `Storage`.
pub fn map_db_error(e: sqlx::Error) -> Error {
    // Transient pool conditions first: no database error is involved.
    if matches!(e, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed) {
        return busy(BUSY_POOL);
    }
    if let sqlx::Error::Database(db) = &e {
        // Postgres trigger codes are chosen by Rusty and classified only by SQLSTATE. Never
        // inspect Postgres message text: caller data may contain strings resembling a code.
        match db.code().as_deref() {
            Some("RS001") => return Error::Conflict("database conflict".into()),
            Some("RS002") => return Error::Invalid("invalid value".into()),
            // Transient classes (D1). 40P01/40001 reach this function only after the retry loop
            // gave up (or outside it): the caller may retry later.
            Some("55P03") => return busy("lock timeout"),
            Some("57014") => return busy("statement timeout"),
            Some("53300") => return busy("too many connections"),
            Some("40P01") | Some("40001") => return busy("transaction conflict"),
            Some(code) if sqlite_busy_code(code) => return busy("database locked"),
            _ => {}
        }
        use sqlx::error::ErrorKind;
        match db.kind() {
            ErrorKind::UniqueViolation => return Error::Conflict("already exists".into()),
            ErrorKind::ForeignKeyViolation => {
                return Error::Conflict("referenced row missing or still in use".into());
            }
            ErrorKind::CheckViolation | ErrorKind::NotNullViolation => {
                return Error::Invalid("invalid value".into());
            }
            _ => {}
        }
        // SQLITE_CONSTRAINT_TRIGGER (RAISE(ABORT, ..)) is not classified by sqlx.
        if db.code().as_deref() == Some("1811") {
            let msg = db.message();
            return match msg.strip_prefix("conflict:") {
                Some(rest) => Error::Conflict(rest.trim().to_string()),
                None => Error::Invalid(msg.to_string()),
            };
        }
    }
    Error::storage_with("database error", e)
}

/// Fixed reason for pool-acquire timeout / closed pool.
pub(crate) const BUSY_POOL: &str = "connection pool unavailable";

/// A transient failure with a fixed, caller-data-free reason (D1).
pub(crate) fn busy(reason: &str) -> Error {
    Error::Busy(reason.to_owned())
}

/// SQLite result codes SQLITE_BUSY (5) and SQLITE_LOCKED (6), including extended codes
/// (`code & 0xff`). SQLite codes are at most four digits; Postgres SQLSTATEs are five
/// characters, so a Postgres code can never be taken for one.
fn sqlite_busy_code(code: &str) -> bool {
    code.len() <= 4
        && code
            .parse::<u32>()
            .is_ok_and(|c| matches!(c & 0xff, 5 | 6))
}

/// SQLite write transaction (`BEGIN IMMEDIATE`).
///
/// Internal: used by `JobQueue::fence` / `complete_in` and their tests; it is removed in PG5
/// when those become `JobFence` parameters of the repository operations.
pub struct SqliteTx(Transaction<'static, Sqlite>);

impl SqliteTx {
    /// Begin a write transaction (`BEGIN IMMEDIATE`, see [`SqliteStore::begin_write`]).
    pub async fn begin(store: &SqliteStore) -> Result<SqliteTx> {
        Ok(SqliteTx(store.begin_write().await?))
    }

    pub fn conn(&mut self) -> &mut sqlx::SqliteConnection {
        &mut self.0
    }

    /// Commit.
    pub async fn commit(self) -> Result<()> {
        self.0.commit().await.map_err(map_db_error)
    }

    /// Roll back. Dropping the transaction has the same effect.
    pub async fn rollback(self) -> Result<()> {
        self.0.rollback().await.map_err(map_db_error)
    }
}
