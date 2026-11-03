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
