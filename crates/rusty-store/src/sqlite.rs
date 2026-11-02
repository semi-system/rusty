use std::str::FromStr;

use rusty_core::{Error, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

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
            .map_err(|e| Error::storage_with(format!("invalid database url {url:?}"), e))?
            .foreign_keys(true)
            .create_if_missing(true);
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        let pool = SqlitePoolOptions::new()
            .max_connections(if in_memory { 1 } else { 4 })
            .min_connections(if in_memory { 1 } else { 0 })
            .idle_timeout(None)
            .max_lifetime(None)
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
