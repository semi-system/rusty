//! `[store]` section of the startup configuration (U11): which database backs the server and how
//! to reach it. Kept apart from the other sections so that it is the only file that knows the
//! Postgres connection policy.
//!
//! Rules (fail closed, error texts never echo values):
//! * `backend = "sqlite"` (default) uses the top-level `database_path`; every Postgres-only key is
//!   refused so a half-converted file cannot be misread.
//! * `backend = "postgres"` needs `url` WITHOUT a password and without query parameters
//!   (`postgres://rusty_app@db.internal:5432/rusty`); the password comes from `password_file`
//!   (a 0600 file read through the `SecretSource`). TLS is configured by `sslmode` and
//!   `ca_bundle`, never by URL parameters.
//! * A production start refuses anything but `verify-full` against a non-loopback host unless
//!   `insecure_allow_unverified_tls = true` (loud warning at start-up). `sslmode = "disable"` is
//!   accepted on loopback by the dev profile only.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::config::Profile;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StoreBackend {
    #[default]
    Sqlite,
    Postgres,
}

/// TLS mode of the database connection (the libpq names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
pub enum SslMode {
    #[serde(rename = "disable")]
    Disable,
    #[serde(rename = "require")]
    Require,
    #[serde(rename = "verify-ca")]
    VerifyCa,
    #[default]
    #[serde(rename = "verify-full")]
    VerifyFull,
}

pub const MAX_POOL_SIZE: u32 = 200;
/// Connections kept beyond one per service (a hot mailbox, a maintenance statement).
pub const POOL_HEADROOM: u32 = 4;
pub const DEFAULT_POOL_SIZE: u32 = 16;
const MAX_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreSection {
    #[serde(default)]
    pub backend: StoreBackend,
    /// `postgres://user@host:port/database`: no password, no query parameters.
    pub url: Option<String>,
    /// File holding the password of the runtime role (0600, read through the `SecretSource`).
    pub password_file: Option<PathBuf>,
    /// Role used by `rustyctl db migrate` (DDL). Never used by the daemon.
    pub migrator_user: Option<String>,
    pub migrator_password_file: Option<PathBuf>,
    /// Connections in the pool; at least the floor for the enabled services.
    #[serde(default = "d_pool")]
    pub pool_size: u32,
    #[serde(default = "d_acquire")]
    pub acquire_timeout_ms: u64,
    /// Longest wait for a row lock, applied per transaction (`SET LOCAL`).
    #[serde(default = "d_lock")]
    pub lock_timeout_ms: u64,
    /// Upper bound the daemon accepts for the runtime role's `statement_timeout` (the value itself
    /// is set on the role: `ALTER ROLE .. SET`, see deploy/db/roles.sql). A role without a timeout,
    /// or with a longer one, makes the start-up fail.
    #[serde(default = "d_statement")]
    pub statement_timeout_ms: u64,
    /// Same for `idle_in_transaction_session_timeout`.
    #[serde(default = "d_idle_tx")]
    pub idle_in_transaction_timeout_ms: u64,
    /// Default `verify-full`.
    #[serde(default)]
    pub sslmode: SslMode,
    /// PEM file with the CA certificate(s) that sign the server certificate.
    pub ca_bundle: Option<PathBuf>,
    /// Explicit, loud opt-out of certificate verification towards a non-loopback host.
    #[serde(default)]
    pub insecure_allow_unverified_tls: bool,
}

fn d_pool() -> u32 {
    DEFAULT_POOL_SIZE
}
fn d_acquire() -> u64 {
    5_000
}
fn d_lock() -> u64 {
    10_000
}
fn d_statement() -> u64 {
    30_000
}
fn d_idle_tx() -> u64 {
    30_000
}

impl Default for StoreSection {
    fn default() -> Self {
        StoreSection {
            backend: StoreBackend::Sqlite,
            url: None,
            password_file: None,
            migrator_user: None,
            migrator_password_file: None,
            pool_size: d_pool(),
            acquire_timeout_ms: d_acquire(),
            lock_timeout_ms: d_lock(),
            statement_timeout_ms: d_statement(),
            idle_in_transaction_timeout_ms: d_idle_tx(),
            sslmode: SslMode::default(),
            ca_bundle: None,
            insecure_allow_unverified_tls: false,
        }
    }
}

/// The parts of a validated Postgres URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgEndpoint {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

fn invalid(msg: &str) -> Error {
    Error::Invalid(msg.into())
}

/// Parse `postgres://user@host[:port]/database`. Refuses a password, query parameters, a fragment,
/// percent escapes and anything that is not a plain TCP endpoint.
pub fn parse_pg_url(url: &str) -> Result<PgEndpoint> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| invalid("[store] url must start with postgres://"))?;
    if rest.contains(['?', '#', '%']) {
        return Err(invalid("[store] url must not carry query parameters or escapes (use the [store] keys)"));
    }
    let (authority, database) = rest
        .split_once('/')
        .ok_or_else(|| invalid("[store] url needs a database name"))?;
    if database.is_empty() || database.contains('/') || database.len() > 63 {
        return Err(invalid("[store] url database name is not valid"));
    }
    let (user, hostport) = authority
        .split_once('@')
        .ok_or_else(|| invalid("[store] url needs a user name"))?;
    if user.is_empty() || user.len() > 63 || user.contains(':') || user.contains('@') {
        return Err(invalid("[store] url must not contain a password; use password_file"));
    }
    let (host, port) = split_host_port(hostport)?;
    let ident_ok = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    if !ident_ok(user) || !ident_ok(database) {
        return Err(invalid("[store] url user or database name has unsupported characters"));
    }
    Ok(PgEndpoint { user: user.to_owned(), host, port, database: database.to_owned() })
}

fn split_host_port(s: &str) -> Result<(String, u16)> {
    let bad = || invalid("[store] url host is not valid");
    let (host, port) = if let Some(v6) = s.strip_prefix('[') {
        let (h, rest) = v6.split_once(']').ok_or_else(bad)?;
        let port = match rest {
            "" => None,
            r => Some(r.strip_prefix(':').ok_or_else(bad)?),
        };
        (h.to_owned(), port)
    } else {
        match s.split_once(':') {
            Some((h, p)) => (h.to_owned(), Some(p)),
            None => (s.to_owned(), None),
        }
    };
    if host.is_empty() || host.len() > 253 {
        return Err(bad());
    }
    let host_ok = host.parse::<IpAddr>().is_ok()
        || host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if !host_ok || host.starts_with('/') {
        return Err(bad());
    }
    let port = match port {
        None => 5432,
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| invalid("[store] url port is not valid"))?,
    };
    Ok((host, port))
}

/// Loopback host (`localhost`, 127.0.0.0/8, ::1).
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl StoreSection {
    pub fn is_postgres(&self) -> bool {
        self.backend == StoreBackend::Postgres
    }

    /// The endpoint of a Postgres store (`None` for SQLite).
    pub fn endpoint(&self) -> Option<Result<PgEndpoint>> {
        match (self.backend, &self.url) {
            (StoreBackend::Postgres, Some(u)) => Some(parse_pg_url(u)),
            _ => None,
        }
    }

    pub fn acquire_timeout(&self) -> Duration {
        Duration::from_millis(self.acquire_timeout_ms)
    }
    pub fn lock_timeout(&self) -> Duration {
        Duration::from_millis(self.lock_timeout_ms)
    }
    pub fn statement_timeout(&self) -> Duration {
        Duration::from_millis(self.statement_timeout_ms)
    }
    pub fn idle_in_transaction_timeout(&self) -> Duration {
        Duration::from_millis(self.idle_in_transaction_timeout_ms)
    }

    /// File-level checks, independent of the profile.
    pub(crate) fn validate(&self) -> Result<()> {
        match self.backend {
            StoreBackend::Sqlite => {
                let pg_only = self.url.is_some()
                    || self.password_file.is_some()
                    || self.migrator_user.is_some()
                    || self.migrator_password_file.is_some()
                    || self.ca_bundle.is_some()
                    || self.insecure_allow_unverified_tls
                    || self.sslmode != SslMode::default()
                    || self.pool_size != d_pool()
                    || self.acquire_timeout_ms != d_acquire()
                    || self.lock_timeout_ms != d_lock()
                    || self.statement_timeout_ms != d_statement()
                    || self.idle_in_transaction_timeout_ms != d_idle_tx();
                if pg_only {
                    return Err(invalid("[store] keys other than backend apply to backend = \"postgres\" only"));
                }
                Ok(())
            }
            StoreBackend::Postgres => self.validate_postgres(),
        }
    }

    fn validate_postgres(&self) -> Result<()> {
        let url = self.url.as_deref().ok_or_else(|| invalid("[store] url is required for backend = \"postgres\""))?;
        parse_pg_url(url)?;
        for (p, what) in [
            (&self.password_file, "password_file"),
            (&self.migrator_password_file, "migrator_password_file"),
            (&self.ca_bundle, "ca_bundle"),
        ] {
            if p.as_deref().is_some_and(|p| !p.is_absolute()) {
                return Err(Error::Invalid(format!("[store] {what} must be an absolute path")));
            }
        }
        if let Some(u) = &self.migrator_user {
            let ok = !u.is_empty()
                && u.len() <= 63
                && u.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
            if !ok {
                return Err(invalid("[store] migrator_user is not a valid role name"));
            }
        }
        if self.migrator_password_file.is_some() && self.migrator_user.is_none() {
            return Err(invalid("[store] migrator_password_file needs migrator_user"));
        }
        if !(1..=MAX_POOL_SIZE).contains(&self.pool_size) {
            return Err(invalid("[store] pool_size must be 1..=200"));
        }
        for (v, what) in [
            (self.acquire_timeout_ms, "acquire_timeout_ms"),
            (self.lock_timeout_ms, "lock_timeout_ms"),
            (self.statement_timeout_ms, "statement_timeout_ms"),
            (self.idle_in_transaction_timeout_ms, "idle_in_transaction_timeout_ms"),
        ] {
            if v == 0 || v > MAX_TIMEOUT_MS {
                return Err(Error::Invalid(format!("[store] {what} must be 1..=3600000")));
            }
        }
        if self.ca_bundle.is_some() && matches!(self.sslmode, SslMode::Disable | SslMode::Require) {
            return Err(invalid("[store] ca_bundle needs sslmode verify-ca or verify-full"));
        }
        Ok(())
    }

    /// The smallest sensible pool for `services` running services: one connection each plus
    /// [`POOL_HEADROOM`] for a hot mailbox and maintenance.
    pub fn pool_floor(services: usize) -> u32 {
        u32::try_from(services).unwrap_or(u32::MAX).saturating_add(POOL_HEADROOM)
    }

    /// Start-up rules that need the profile and the number of running services. Returns the
    /// warning to log, if any (an explicit insecure override).
    pub fn check_startup(&self, profile: Profile, services: usize) -> Result<Option<&'static str>> {
        if self.backend != StoreBackend::Postgres {
            return Ok(None);
        }
        let ep = parse_pg_url(self.url.as_deref().unwrap_or_default())?;
        if self.pool_size < Self::pool_floor(services) {
            return Err(invalid("[store] pool_size is below the floor for the enabled services (services + 4)"));
        }
        if self.password_file.is_none() && profile == Profile::Production {
            return Err(invalid("[store] password_file is required in production"));
        }
        let loopback = is_loopback_host(&ep.host);
        let verified = self.sslmode == SslMode::VerifyFull;
        let allowed_unverified = match (profile, loopback) {
            // Loopback is not on the wire between hosts. `disable` is a dev convenience only.
            (_, true) => self.sslmode != SslMode::Disable || profile == Profile::Dev,
            (_, false) => false,
        };
        if verified || allowed_unverified {
            return Ok(None);
        }
        if self.insecure_allow_unverified_tls {
            return Ok(Some(
                "database TLS is NOT verified (insecure_allow_unverified_tls = true): the connection can be intercepted",
            ));
        }
        Err(invalid(
            "[store] sslmode must be verify-full (with ca_bundle if the CA is private) for this database host; \
             set insecure_allow_unverified_tls = true only if you accept the risk",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parts() {
        let e = parse_pg_url("postgres://rusty_app@db.internal:6432/rusty").unwrap();
        assert_eq!((e.user.as_str(), e.host.as_str(), e.port, e.database.as_str()), ("rusty_app", "db.internal", 6432, "rusty"));
        assert_eq!(parse_pg_url("postgresql://u@[::1]/d").unwrap().host, "::1");
        assert_eq!(parse_pg_url("postgres://u@h/d").unwrap().port, 5432);
    }

    #[test]
    fn url_refusals() {
        for bad in [
            "postgres://u:secret@h/d",
            "postgres://u@h/d?sslmode=disable",
            "postgres://u@h/d#x",
            "postgres://u@h",
            "postgres://@h/d",
            "postgres://h/d",
            "postgres://u@h:0/d",
            "postgres://u@h:99999/d",
            "postgres://u@/d",
            "mysql://u@h/d",
            "postgres://u@h/d/e",
            "postgres://u%40@h/d",
            "postgres://u@/var/run/postgresql/d",
        ] {
            assert!(parse_pg_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loopback() {
        for h in ["localhost", "127.0.0.1", "127.9.9.9", "::1"] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in ["db", "10.0.0.5", "0.0.0.0", "localhost.evil.test"] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }
}
