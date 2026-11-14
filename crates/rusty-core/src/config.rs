//! Startup configuration (spec §6): a small TOML file read once at process start.
//!
//! Error texts never echo file contents (they may sit next to secrets).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};
use serde::Deserialize;

use crate::{Error, Result};

pub use crate::config_role::{Role, UNUSED_BLOB_DIR};
pub use crate::config_sealing::{SealingConfig, UnsealingConfig, MAX_UNSEALING_KEYS};
pub use crate::config_valkey::ValkeyConfig;
pub use crate::config_store::{
    is_loopback_host, parse_pg_url, PgEndpoint, SslMode, StoreSection, DEFAULT_POOL_SIZE, MAX_POOL_SIZE,
    POOL_HEADROOM,
};
pub use crate::config_sections::{
    valid_hostname, DnsCheckConfig, ImapConfig, LimitsConfig, Listen, LogFormat, Nameserver, OutboundConfig, RelayConfig,
    RelayTls, SmtpConfig, TlsConfig, TlsPolicy, DNS_CHECK_JOB_KIND,
};

/// The Postgres server of the dev profile when `RUSTY_DEV_PG_URL` does not name another one.
pub const DEV_DEFAULT_PG_URL: &str = "postgres://rusty@127.0.0.1:5432/rusty";

/// Largest configuration file accepted by [`StartupConfig::load`].
pub const MAX_CONFIG_FILE_BYTES: u64 = 64 * 1024;
pub(crate) const MIN_MESSAGE_BYTES: u64 = 1024;
const MAX_MESSAGE_BYTES: u64 = 1024 * 1024 * 1024;
const MIN_GC_GRACE_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

/// Startup settings.
///
/// Filesystem expectations (security baseline, see SECURITY.md): the service runs as its own
/// account with umask 077. The `blob_dir` is owned by that account with mode 0700; it must be
/// prepared once with `FsBlobStore::init` and is refused by `FsBlobStore::open` when it is missing,
/// unmarked, a symlink, or writable by group or others. Loading this file checks none of this. The
/// metadata lives in Postgres (`[store]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
// Adding a top-level key: list it in `Role::own` of the services that use it (a key no role lists is refused by every
// role: default deny), in `SCHEMA_KEYS` and in the matrix of tests/config_roles.rs.
pub struct StartupConfig {
    /// Absolute path of the blob root (mode 0700, initialized with `FsBlobStore::init`;
    /// never created implicitly).
    pub blob_dir: PathBuf,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default = "default_max_message_bytes")]
    pub max_message_bytes: u64,
    #[serde(default = "default_gc_grace_seconds")]
    pub gc_grace_seconds: u64,
    /// Fully qualified name of this server (greeting, certificate SAN, DKIM). Required unless
    /// `rusty d --dev`.
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub log_format: LogFormat,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub smtp: SmtpConfig,
    #[serde(default)]
    pub imap: ImapConfig,
    #[serde(default)]
    pub outbound: OutboundConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    /// `[dns_check]`: periodic DNS re-validation (off by default).
    #[serde(default)]
    pub dns_check: DnsCheckConfig,
    #[serde(default)]
    pub store: StoreSection,
    /// `[valkey]`: the shared key-value store (rate limits, sessions, read cache); absent = every consumer keeps
    /// its own in-process state.
    #[serde(default)]
    pub valkey: Option<ValkeyConfig>,
    /// `[sealing]`: the public key secrets are sealed to (held by whatever seals: the admin service, `rustyctl`).
    #[serde(default)]
    pub sealing: Option<SealingConfig>,
    /// `[unsealing]`: the files holding the private keys that open sealed secrets (the outbound service only).
    #[serde(default)]
    pub unsealing: Option<UnsealingConfig>,
}

/// Which start-up rules apply (see [`StartupConfig::check_startup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Production,
    Dev,
}

fn default_max_message_bytes() -> u64 {
    52_428_800
}

fn default_gc_grace_seconds() -> u64 {
    604_800
}

/// Read at most `max` bytes from `r`. One extra byte is requested so that input longer than
/// `max` is detected and rejected (`Invalid`, "too large") instead of silently truncated.
fn read_bounded<R: Read>(r: R, max: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(max as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::storage_with("cannot read configuration file", e))?;
    if buf.len() > max {
        return Err(Error::Invalid("configuration file is too large".into()));
    }
    Ok(buf)
}

impl StartupConfig {
    /// Parse and validate. Syntax and schema errors are `Invalid` without the offending text.
    pub fn from_toml_str(s: &str) -> Result<StartupConfig> {
        let mut c: StartupConfig = toml::from_str(s)
            .map_err(|_| Error::Invalid("configuration is not valid (syntax or schema)".into()))?;
        c.validate()?;
        Ok(c)
    }

    /// Read at most [`MAX_CONFIG_FILE_BYTES`] from `path` and parse it.
    pub fn load(path: &Path) -> Result<StartupConfig> {
        Self::load_with(path, Self::from_toml_str)
    }

    /// The bounded read of a configuration file, parsed by `parse`.
    pub(crate) fn load_with(path: &Path, parse: impl FnOnce(&str) -> Result<StartupConfig>) -> Result<StartupConfig> {
        // O_NONBLOCK so opening a FIFO (or similar) cannot hang the process; the fstat on the
        // opened descriptor (no path race) then rejects anything that is not a regular file.
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| match std::io::Error::from(e) {
            e if e.kind() == std::io::ErrorKind::NotFound => {
                Error::NotFound("configuration file".into())
            }
            e => Error::storage_with("cannot open configuration file", e),
        })?;
        let st = rustix::fs::fstat(&fd)
            .map_err(|e| Error::storage_with("cannot stat configuration file", std::io::Error::from(e)))?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
            || st.st_size < 0
            || st.st_size as u64 > MAX_CONFIG_FILE_BYTES
        {
            return Err(Error::Invalid("configuration file is not a small regular file".into()));
        }
        let file = File::from(fd);
        // Bounded even if the file grows after the fstat check.
        let buf = read_bounded(file, MAX_CONFIG_FILE_BYTES as usize)?;
        let text = String::from_utf8(buf)
            .map_err(|_| Error::Invalid("configuration file is not UTF-8".into()))?;
        parse(&text)
    }

    pub(crate) fn validate(&mut self) -> Result<()> {
        self.store.validate()?;
        if !self.blob_dir.is_absolute() {
            return Err(Error::Invalid("blob_dir must be absolute".into()));
        }
        if !(MIN_MESSAGE_BYTES..=MAX_MESSAGE_BYTES).contains(&self.max_message_bytes) {
            return Err(Error::Invalid("max_message_bytes must be 1 KiB..=1 GiB".into()));
        }
        if self.gc_grace_seconds < MIN_GC_GRACE_SECONDS {
            return Err(Error::Invalid("gc_grace_seconds must be at least 3600".into()));
        }
        if let Some(h) = &self.hostname {
            if !valid_hostname(h) {
                return Err(Error::Invalid("hostname is not a valid host name".into()));
            }
        }
        self.tls.validate()?;
        self.smtp.validate(self.max_message_bytes)?;
        self.imap.validate()?;
        self.dns_check.validate()?;
        self.outbound.validate()?;
        // Queued message bodies carry no database reference (see crates/rusty-smtp/tests/outbound_gc.rs):
        // the blob GC must not be allowed to collect one before the queue gives up on it.
        if self.gc_grace_seconds < self.outbound.max_age_secs.saturating_add(86_400) {
            return Err(Error::Invalid("gc_grace_seconds must be at least outbound.max_age_secs plus one day".into()));
        }
        if let Some(v) = &self.valkey {
            v.validate()?;
        }
        if let Some(s) = &self.sealing {
            s.validate()?;
        }
        if let Some(u) = &self.unsealing {
            u.validate()?;
        }
        self.limits.validate()
    }

    /// The SMTP message size cap: `[smtp] max_message_bytes` or the top-level value.
    pub fn smtp_max_message_bytes(&self) -> u64 {
        self.smtp.max_message_bytes.unwrap_or(self.max_message_bytes)
    }

    /// The listeners that accept authentication (submission and IMAP). Port 25 never does.
    pub fn authenticated_listeners(&self) -> usize {
        [self.smtp.listen_587, self.smtp.listen_465, self.imap.listen_143, self.imap.listen_993]
            .iter()
            .filter(|l| l.addr().is_some())
            .count()
    }

    /// Start-up rules that need the whole file: a production start needs `hostname`, and an
    /// enabled listener that accepts authentication needs a TLS source (a certificate pair or
    /// the explicit `dev_self_signed`). There is no plaintext fallback: the start is refused.
    pub fn check_startup(&self, profile: Profile) -> Result<()> {
        if profile == Profile::Production && self.hostname.is_none() {
            return Err(Error::Invalid("hostname is required".into()));
        }
        if self.authenticated_listeners() > 0 && !self.tls.has_cert() && !self.tls.dev_self_signed {
            return Err(Error::Invalid(
                "listeners that accept authentication need TLS: set [tls] cert_file and key_file, \
                 or disable those listeners"
                    .into(),
            ));
        }
        self.store.check_startup(profile, self.service_count())?;
        if let Some(v) = &self.valkey {
            v.check_startup(profile)?;
        }
        Ok(())
    }

    /// Services the process runs: every enabled listener, the GC ticker and the outbound worker.
    pub fn service_count(&self) -> usize {
        let listeners = [
            self.smtp.listen_25,
            self.smtp.listen_587,
            self.smtp.listen_465,
            self.imap.listen_143,
            self.imap.listen_993,
        ]
        .iter()
        .filter(|l| l.addr().is_some())
        .count();
        listeners + 2
    }

    /// Warnings the start-up must log (an explicit insecure database TLS override).
    pub fn startup_warnings(&self, profile: Profile) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if let Ok(Some(w)) = self.store.check_startup(profile, self.service_count()) {
            warnings.push(w);
        }
        if let Some(Ok(Some(w))) = self.valkey.as_ref().map(|v| v.check_startup(profile)) {
            warnings.push(w);
        }
        warnings
    }

    /// Dev profile only: point the `[store]` section at the Postgres server `url` (`postgres://user@host:port/db`,
    /// no password: that comes from `RUSTY_DEV_PG_PASSWORD`). Without `sslmode` a loopback host is reached without
    /// TLS and any other host keeps the default `verify-full`; `ca_bundle` is the CA that signs the server
    /// certificate. Naming a mode that does not verify the server for a remote host is the developer's explicit
    /// choice: it turns on `insecure_allow_unverified_tls` (a warning at every start).
    pub fn with_dev_postgres(
        mut self,
        url: &str,
        sslmode: Option<SslMode>,
        ca_bundle: Option<PathBuf>,
    ) -> Result<StartupConfig> {
        let ep = crate::config_store::parse_pg_url(url)?;
        let loopback = is_loopback_host(&ep.host);
        self.store.url = Some(url.to_owned());
        self.store.password_from_dev_env = true;
        self.store.sslmode = sslmode.unwrap_or(if loopback { SslMode::Disable } else { SslMode::VerifyFull });
        self.store.ca_bundle = ca_bundle;
        self.store.insecure_allow_unverified_tls = !loopback && self.store.sslmode != SslMode::VerifyFull;
        self.validate()?;
        Ok(self)
    }

    /// The `rusty d --dev` profile: everything under `data_dir`, high ports on loopback and
    /// the explicit dev certificate. `data_dir` should be absolute.
    pub fn dev(data_dir: &Path) -> StartupConfig {
        StartupConfig {
            blob_dir: data_dir.join("blobs"),
            log_level: LogLevel::default(),
            max_message_bytes: default_max_message_bytes(),
            gc_grace_seconds: default_gc_grace_seconds(),
            hostname: Some("localhost".into()),
            log_format: LogFormat::default(),
            tls: TlsConfig { cert_file: None, key_file: None, dev_self_signed: true },
            smtp: SmtpConfig {
                listen_25: Listen::loopback(2525),
                listen_587: Listen::loopback(5587),
                listen_465: Listen::loopback(4465),
                ..SmtpConfig::default()
            },
            imap: ImapConfig {
                listen_143: Listen::loopback(1143),
                listen_993: Listen::loopback(9993),
                ..ImapConfig::default()
            },
            outbound: OutboundConfig::default(),
            limits: LimitsConfig::default(),
            dns_check: DnsCheckConfig::default(),
            store: StoreSection {
                url: Some(DEV_DEFAULT_PG_URL.to_owned()),
                sslmode: SslMode::Disable,
                password_from_dev_env: true,
                ..StoreSection::default()
            },
            valkey: None,
            sealing: None,
            unsealing: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read};

    fn is_invalid<T: std::fmt::Debug>(r: Result<T>) -> bool {
        matches!(r, Err(Error::Invalid(_)))
    }

    #[test]
    fn read_bounded_rejects_one_over_the_limit() {
        let max = MAX_CONFIG_FILE_BYTES as usize;
        let r = read_bounded(io::repeat(b'#').take(max as u64 + 1), max);
        match r {
            Err(Error::Invalid(m)) => assert!(m.contains("too large"), "{m}"),
            other => panic!("{other:?}"),
        }
        // An endless reader is still cut off.
        assert!(is_invalid(read_bounded(io::repeat(b'#'), max)));
    }

    #[test]
    fn read_bounded_accepts_exact_limit_and_empty() {
        let max = MAX_CONFIG_FILE_BYTES as usize;
        assert_eq!(read_bounded(io::repeat(b'#').take(max as u64), max).unwrap().len(), max);
        assert!(read_bounded(io::empty(), max).unwrap().is_empty());
    }

    #[test]
    fn read_bounded_propagates_io_errors() {
        struct Bad;
        impl Read for Bad {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        assert!(matches!(read_bounded(Bad, 10), Err(Error::Storage { .. })));
    }
}
