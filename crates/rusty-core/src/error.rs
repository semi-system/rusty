//! The shared error type.
//!
//! Rules (spec §8b):
//! - The message of every variant is a short, fixed reason. It never echoes caller input
//!   (addresses, domains, names, URLs, paths, hashes, ids, header text), so `Display` of an
//!   `Error` is safe to log and to map to a protocol-level status.
//! - The `source()` chain of `Error::Storage` may contain raw backend text (SQLite
//!   messages, OS errors with paths, parser details). It is for operator diagnostics only and
//!   must never be serialized to clients.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("quota exceeded: {0}")]
    Quota(String),
    #[error("invalid: {0}")]
    Invalid(String),
    /// Transient storage failure: the operation did not happen (or its outcome is not known to
    /// be committed, see the store docs) and may be retried later. Lock or statement timeout,
    /// connection pool exhausted or closed, too many connections, or a deadlock / serialization
    /// failure that outlived the bounded retries. The text is a fixed reason, never raw database
    /// text, a URL or caller data. Distinct from [`Error::Storage`] (corruption, unexpected
    /// failure): a daemon answers `Busy` with a retryable status (SMTP 451 4.3.0, HTTP 503 with
    /// `Retry-After`, IMAP `[UNAVAILABLE]`), and `Storage` with a permanent or internal-error one.
    ///
    /// Mapping note (decision pending, deliberately unchanged): Postgres admin shutdown / crash
    /// recovery (57P01, 57P02, 57P03), idle-in-transaction timeout (25P03) and connection
    /// exceptions (08xxx) map to [`Error::Storage`], as the D1 list does not name them. For a
    /// failover-oriented deployment `Busy` is arguably the better class for 57P0x; revisit in PG8
    /// together with the failover tests. `25006`
    /// (read_only_sql_transaction: a write reached a standby) already maps to `Busy`: the statement did
    /// not happen and a retry reaches the new primary. Single-statement pool operations run without
    /// `lock_timeout` (only write transactions set it) and are bounded by `statement_timeout`.
    #[error("busy: {0}")]
    Busy(String),
    #[error("storage: {message}")]
    Storage {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl Error {
    pub fn storage(message: impl Into<String>) -> Self {
        Error::Storage { message: message.into(), source: None }
    }

    pub fn storage_with(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Error::Storage { message: message.into(), source: Some(Box::new(source)) }
    }
}
