//! PostgreSQL connection infrastructure: a bounded pool, the TLS policy, the runtime-role and
//! schema-version checks, and the per-mailbox write queue.
//!
//! The daemon connects with the runtime role (`rusty_app`, DML only) and NEVER changes the schema:
//! [`PgStore::connect_with`] checks that the database is at exactly the schema version this binary
//! embeds and refuses to run otherwise (`rustyctl db migrate`, with the separate migrator role,
//! changes the schema: see `migrate.rs`). Nothing here sets session state on a connection:
//! `statement_timeout` and `idle_in_transaction_session_timeout` are properties of the role
//! (`ALTER ROLE .. SET`, deploy/db/roles.sql) and are only VERIFIED; `lock_timeout` is applied
//! per transaction (`SET LOCAL`). That keeps the code correct behind a transaction-pooling proxy.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::{is_loopback_host, SslMode};
use rusty_core::{Error, MailboxId, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Postgres, Transaction};

use crate::pg_queue::{MailboxPermit, MailboxQueue};

/// Settings applied to every PostgreSQL connection pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgLimits {
    pub max_connections: u32,
    /// Longest wait for a pooled connection before the operation fails with `Error::Busy`.
    pub acquire_timeout: Duration,
    /// Largest `statement_timeout` the runtime role may have. The role carries the value
    /// (`ALTER ROLE .. SET`); with [`PgSecurity::verify_role`] a role without a timeout, or with a
    /// longer one, is refused at connect time. Nothing is set on the session.
    pub statement_timeout: Duration,
    /// Same for `idle_in_transaction_session_timeout`.
    pub idle_in_transaction_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
    /// Longest wait for a row lock before the operation fails with `Error::Busy` (55P03). Applied
    /// per transaction (`set_config(.., true)`, i.e. SET LOCAL), not per session, so it survives a
    /// transaction-pooling proxy.
    pub lock_timeout: Duration,
    /// Longest wait in the per-mailbox write queue (before a connection is even requested).
    pub queue_timeout: Duration,
}

impl Default for PgLimits {
    fn default() -> Self {
        Self {
            max_connections: 4,
            acquire_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(30),
            idle_in_transaction_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(5 * 60),
            max_lifetime: Duration::from_secs(30 * 60),
            lock_timeout: Duration::from_secs(10),
            queue_timeout: Duration::from_secs(10),
        }
    }
}

/// A database password. Debug never shows it; it is not part of any error.
#[derive(Clone)]
pub struct PgPassword(String);

impl PgPassword {
    pub fn new(secret: String) -> Self {
        PgPassword(secret)
    }
}

impl fmt::Debug for PgPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PgPassword(<redacted>)")
    }
}

/// Transport and role policy of a Postgres connection.
#[derive(Clone, Debug, Default)]
pub struct PgSecurity {
    /// TLS mode; `None` takes the mode the URL names (libpq default `prefer`).
    pub ssl_mode: Option<SslMode>,
    /// PEM bytes of the CA certificate(s) that sign the server certificate.
    pub ca_bundle_pem: Option<Vec<u8>>,
    /// Explicit opt-out: allow a non-loopback host without `verify-full`.
    pub allow_unverified_tls: bool,
    /// Password of the connecting role (kept out of the URL).
    pub password: Option<PgPassword>,
    /// Refuse a role that is not a plain runtime role (superuser, DDL rights, TRUNCATE, writes on
    /// the address registry) or has no statement / idle-in-transaction timeout of its own.
    pub verify_role: bool,
}

/// A bounded PostgreSQL pool. Clones share the same pool and the same write queue.
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
    lock_timeout: String,
    queue: Arc<MailboxQueue>,
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

fn map_ssl(m: SslMode) -> PgSslMode {
    match m {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Require => PgSslMode::Require,
        SslMode::VerifyCa => PgSslMode::VerifyCa,
        SslMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

/// Connection options with the TLS policy applied. The policy is: a host that is not loopback (and
/// not a Unix socket) must be reached with `verify-full`, unless the caller set the explicit
/// override. It is enforced here, below every caller, so no composition root can forget it.
pub(crate) fn connect_options(url: &str, security: &PgSecurity) -> Result<PgConnectOptions> {
    let mut options = PgConnectOptions::from_str(url)
        // Do not retain a parser error that may contain the caller's URL.
        .map_err(|_| Error::storage("invalid Postgres URL"))?
        .application_name("rusty-store");
    if let Some(mode) = security.ssl_mode {
        options = options.ssl_mode(map_ssl(mode));
    }
    if let Some(pem) = &security.ca_bundle_pem {
        options = options.ssl_root_cert_from_pem(pem.clone());
    }
    if let Some(p) = &security.password {
        options = options.password(&p.0);
    }
    let host = options.get_host().to_owned();
    let local = host.starts_with('/') || is_loopback_host(&host);
    if !local && !matches!(options.get_ssl_mode(), PgSslMode::VerifyFull) && !security.allow_unverified_tls {
        return Err(Error::Invalid(
            "Postgres TLS must be verify-full for a non-loopback host".into(),
        ));
    }
    Ok(options)
}

impl PgStore {
    /// Connect with the bounded defaults in [`PgLimits`] and the TLS mode named by the URL.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_config(url, PgLimits::default()).await
    }

    /// Connect with explicit pool limits (no extra security policy).
    pub async fn connect_with_config(url: &str, config: PgLimits) -> Result<Self> {
        Self::connect_with(url, config, &PgSecurity::default()).await
    }

    /// Connect, apply the TLS policy, optionally verify the role, and require the schema version
    /// this binary embeds. Never migrates.
    pub async fn connect_with(url: &str, config: PgLimits, security: &PgSecurity) -> Result<Self> {
        validate_config(&config)?;
        let options = connect_options(url, security)?;
        let lock_timeout = pg_millis(config.lock_timeout)?;
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(config.idle_timeout))
            .max_lifetime(Some(config.max_lifetime))
            .connect_with(options)
            .await
            // Connection errors can contain endpoint details. The protocol-safe error keeps
            // neither the URL nor the sqlx source; operators get endpoint context separately.
            .map_err(|_| Error::storage("cannot connect to Postgres"))?;

        let checked = async {
            if security.verify_role {
                crate::migrate::verify_runtime_role(&pool, &config).await?;
            }
            crate::migrate::require_current_schema(&pool).await
        }
        .await;
        if let Err(e) = checked {
            pool.close().await;
            return Err(e);
        }
        Ok(Self { pool, lock_timeout, queue: Arc::new(MailboxQueue::new(config.queue_timeout)) })
    }

    /// The underlying pool. Public only with the `test-fixtures` feature (dev-dependencies only,
    /// see scripts/check-test-fixtures.sh); production builds keep it crate-private, so raw SQL
    /// access from another crate needs a feature no production build enables.
    #[cfg(feature = "test-fixtures")]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    #[cfg(not(feature = "test-fixtures"))]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Wait for this mailbox's turn among the writers of this process (see `pg_queue.rs`). Call
    /// BEFORE [`begin_write`](Self::begin_write) and keep the permit until the transaction ended.
    pub async fn mailbox_turn(&self, mb: MailboxId) -> Result<MailboxPermit> {
        self.queue.acquire(mb.0).await
    }

    /// The per-mailbox write queue (diagnostics and tests).
    pub fn queue(&self) -> &MailboxQueue {
        &self.queue
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

    /// The feed of changes made by other processes (migration 0017): one dedicated connection that LISTENs.
    /// Nothing connects until it is run.
    pub fn change_feed(&self) -> crate::feed::PgChangeFeed {
        crate::feed::PgChangeFeed::new(&self.pool)
    }

    /// Close all connections and wait for checked-out connections to return.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn validate_config(config: &PgLimits) -> Result<()> {
    if config.max_connections == 0
        || config.acquire_timeout.is_zero()
        || config.statement_timeout.is_zero()
        || config.idle_in_transaction_timeout.is_zero()
        || config.idle_timeout.is_zero()
        || config.max_lifetime.is_zero()
        || config.lock_timeout.is_zero()
        || config.queue_timeout.is_zero()
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

pub(crate) fn pg_millis(value: Duration) -> Result<String> {
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
