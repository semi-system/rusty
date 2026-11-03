//! PostgreSQL connection infrastructure.
//!
//! This module deliberately contains no Rusty schema or backend-specific business
//! operations yet. PG0 establishes a bounded, consistently configured pool; PG1 adds the
//! schema and PG2 adds mailbox-row locking at the repository boundary.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rusty_core::{Error, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};

/// Settings applied to every PostgreSQL connection pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgPoolConfig {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub statement_timeout: Duration,
    pub idle_in_transaction_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
    /// Longest wait for a row lock before the operation fails with `Error::Busy` (55P03). Applied
    /// per transaction (`set_config(.., true)`, i.e. SET LOCAL), not per session, so it survives a
    /// transaction-pooling proxy.
    pub lock_timeout: Duration,
}

impl Default for PgPoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 4,
            acquire_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(30),
            idle_in_transaction_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(5 * 60),
            max_lifetime: Duration::from_secs(30 * 60),
            lock_timeout: Duration::from_secs(10),
        }
    }
}

/// A bounded PostgreSQL pool. Clones share the same pool.
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
    lock_timeout: String,
}

// `sqlx`'s pool debug representation is not part of our credential-leak contract. Keep the
// public representation deliberately small and stable.
impl fmt::Debug for PgStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgStore")
            .field("size", &self.pool.size())
            .field("idle", &self.pool.num_idle())
            .finish()
    }
}

impl PgStore {
    /// Connect with the bounded defaults in [`PgPoolConfig`].
    ///
    /// PG0 supports the loopback test server with `sslmode=disable`. Production TLS transport,
    /// `verify-full` policy and certificate provisioning belong to PG8.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_config(url, PgPoolConfig::default()).await
    }

    /// Connect with explicit pool and session limits.
    pub async fn connect_with_config(url: &str, config: PgPoolConfig) -> Result<Self> {
        validate_config(&config)?;
        let options = PgConnectOptions::from_str(url)
            // Do not retain a parser error that may contain the caller's URL.
            .map_err(|_| Error::storage("invalid Postgres URL"))?
            .application_name("rusty-store");

        let statement_timeout = pg_millis(config.statement_timeout)?;
        let idle_in_transaction_timeout = pg_millis(config.idle_in_transaction_timeout)?;
        let lock_timeout = pg_millis(config.lock_timeout)?;
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(config.idle_timeout))
            .max_lifetime(Some(config.max_lifetime))
            .after_connect(move |conn, _meta| {
                let statement_timeout = statement_timeout.clone();
                let idle_in_transaction_timeout = idle_in_transaction_timeout.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                        .bind(statement_timeout)
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query(
                        "SELECT set_config('idle_in_transaction_session_timeout', $1, false)",
                    )
                    .bind(idle_in_transaction_timeout)
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            // Connection errors can contain endpoint details. The protocol-safe error keeps
            // neither the URL nor the sqlx source; operators get endpoint context separately.
            .map_err(|_| Error::storage("cannot connect to Postgres"))?;

        sqlx::migrate!("./migrations_pg")
            .run(&pool)
            .await
            .map_err(|_| Error::storage("Postgres migration failed"))?;

        Ok(Self { pool, lock_timeout })
    }

    /// The underlying pool, temporarily public for PG0 tests and the backend port.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begin a READ COMMITTED write transaction.
    ///
    /// PostgreSQL's default is READ COMMITTED. PG2 will require callers to lock the target
    /// mailbox row immediately after this call, before any read-then-write operation.
    pub async fn begin_write(&self) -> Result<Transaction<'static, Postgres>> {
        let mut tx = self.pool.begin().await.map_err(begin_error)?;
        // Transaction-local (SET LOCAL semantics): no session state, safe behind a
        // transaction-pooling proxy.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(&self.lock_timeout)
            .execute(&mut *tx)
            .await
            .map_err(begin_error)?;
        Ok(tx)
    }

    /// Close all connections and wait for checked-out connections to return.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn validate_config(config: &PgPoolConfig) -> Result<()> {
    if config.max_connections == 0
        || config.acquire_timeout.is_zero()
        || config.statement_timeout.is_zero()
        || config.idle_in_transaction_timeout.is_zero()
        || config.idle_timeout.is_zero()
        || config.max_lifetime.is_zero()
        || config.lock_timeout.is_zero()
    {
        return Err(Error::Invalid("invalid Postgres pool configuration".into()));
    }
    // PostgreSQL accepts an integer millisecond setting. Reject truncation and values beyond
    // its signed 32-bit GUC representation instead of silently changing the requested limit.
    pg_millis(config.statement_timeout)?;
    pg_millis(config.idle_in_transaction_timeout)?;
    pg_millis(config.lock_timeout)?;
    Ok(())
}

fn pg_millis(value: Duration) -> Result<String> {
    let millis = value.as_millis();
    if millis == 0 || millis > i32::MAX as u128 {
        return Err(Error::Invalid("invalid Postgres timeout".into()));
    }
    Ok(format!("{millis}ms"))
}

/// Map a BEGIN (or first statement) failure: pool exhaustion, a closed pool and "too many
/// connections" are transient (`Busy`); anything else is a fixed `Storage` error. Never raw text.
fn begin_error(e: sqlx::Error) -> Error {
    match crate::map_db_error(e) {
        busy @ Error::Busy(_) => busy,
        _ => Error::storage("cannot begin Postgres transaction"),
    }
}
