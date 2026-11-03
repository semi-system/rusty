use std::str::FromStr;

use rusty_core::{Error, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Sqlite, SqlitePool, Transaction};

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
    pub async fn open(url: &str) -> Result<SqliteStore> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| Error::storage_with("invalid database url", e))?
            .foreign_keys(true)
            .create_if_missing(true);
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
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

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Begin a write transaction with `BEGIN IMMEDIATE`, taking SQLite's write lock up
    /// front so concurrent writers serialize instead of failing on lock upgrade.
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

/// Map a database error to a domain error without leaking raw SQLite text.
/// UNIQUE/PRIMARY KEY -> `Conflict("already exists")`, FK -> `Conflict("referenced row
/// missing or still in use")`, CHECK/NOT NULL -> `Invalid("invalid value")`. Trigger
/// `RAISE(ABORT, ..)` messages (ours) pass through: a `conflict:` prefix gives `Conflict`,
/// anything else `Invalid`. Everything else is `Storage`.
pub fn map_db_error(e: sqlx::Error) -> Error {
    if let sqlx::Error::Database(db) = &e {
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
