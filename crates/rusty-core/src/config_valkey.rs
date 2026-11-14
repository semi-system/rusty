//! `[valkey]`: the shared key-value store used for rate limits, sessions and the read cache.
//!
//! The section is optional. Without it every consumer keeps its own in-process state (today's
//! behaviour), so Valkey is an upgrade for deployments with several replicas, never a requirement.
//! The password is never inline: a file (mode 0600, read through `SecretSource`), like every other
//! secret of the configuration.

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{is_loopback_host, valid_hostname, Profile};
use crate::{Error, Result};

const DEFAULT_PORT: u16 = 6379;
/// Establishing the connection (and re-establishing it after a loss).
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 1000;
const MAX_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// One command. Short on purpose: a rate-limit check sits in the login path, so a slow server must be
/// given up on quickly (the caller then falls back to its in-process state).
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 200;
const MAX_COMMAND_TIMEOUT_MS: u64 = 5000;
const MAX_PREFIX_BYTES: usize = 32;
const MAX_USERNAME_BYTES: usize = 255;

fn d_port() -> u16 {
    DEFAULT_PORT
}
fn d_connect() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn d_command() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}
fn d_prefix() -> String {
    "rusty:".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValkeyConfig {
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    /// TLS to the server (certificate chain and name verified against `ca_bundle`, or the built-in web roots).
    /// `false` is accepted for loopback and, with a warning, in the development profile.
    #[serde(default)]
    pub tls: bool,
    /// CA that signs the server's certificate (absolute path).
    #[serde(default)]
    pub ca_bundle: Option<PathBuf>,
    /// ACL user. Give each service its own (least privilege); without it the `default` user is used.
    #[serde(default)]
    pub username: Option<String>,
    /// File holding the password (absolute path, mode 0600); never inline.
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    #[serde(default = "d_connect")]
    pub connect_timeout_ms: u64,
    #[serde(default = "d_command")]
    pub command_timeout_ms: u64,
    /// Prepended to every key, so that several installations or environments can share one server.
    #[serde(default = "d_prefix")]
    pub key_prefix: String,
}

fn bad(msg: &str) -> Error {
    Error::Invalid(msg.into())
}

impl ValkeyConfig {
    /// Schema checks that need no profile.
    pub(crate) fn validate(&self) -> Result<()> {
        let ip = self.host.parse::<std::net::IpAddr>().is_ok();
        if self.host.is_empty() || (!ip && !valid_hostname(&self.host)) {
            return Err(bad("valkey.host must be a host name or an address (no scheme, no port)"));
        }
        if self.port == 0 {
            return Err(bad("valkey.port must be set"));
        }
        if self.connect_timeout_ms == 0 || self.connect_timeout_ms > MAX_CONNECT_TIMEOUT_MS {
            return Err(bad("valkey.connect_timeout_ms must be between 1 and 10000"));
        }
        if self.command_timeout_ms == 0 || self.command_timeout_ms > MAX_COMMAND_TIMEOUT_MS {
            return Err(bad("valkey.command_timeout_ms must be between 1 and 5000"));
        }
        let prefix_ok = !self.key_prefix.is_empty()
            && self.key_prefix.len() <= MAX_PREFIX_BYTES
            && self.key_prefix.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'.'));
        if !prefix_ok {
            return Err(bad("valkey.key_prefix must be 1 to 32 letters, digits and : - _ ."));
        }
        if self.ca_bundle.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err(bad("valkey.ca_bundle must be an absolute path"));
        }
        match (&self.username, &self.password_file) {
            (None, None) => {}
            (Some(u), Some(p)) => {
                if u.is_empty() || u.len() > MAX_USERNAME_BYTES || u.bytes().any(|b| b.is_ascii_control() || b == b' ') {
                    return Err(bad("valkey.username is not valid"));
                }
                if !p.is_absolute() {
                    return Err(bad("valkey.password_file must be an absolute path"));
                }
            }
            (None, Some(p)) if p.is_absolute() => {} // the `default` user with a password
            (None, Some(_)) => return Err(bad("valkey.password_file must be an absolute path")),
            (Some(_), None) => return Err(bad("valkey.username and password_file must be set together")),
        }
        Ok(())
    }

    /// Start-up rules. A link without TLS is for loopback and development; in production it is refused,
    /// in development it yields a warning for the log.
    pub(crate) fn check_startup(&self, profile: Profile) -> Result<Option<&'static str>> {
        if self.tls || is_loopback_host(&self.host) {
            return Ok(None);
        }
        match profile {
            Profile::Production => Err(bad(
                "valkey is reached without tls on a host that is not loopback: set valkey.tls = true",
            )),
            Profile::Dev => Ok(Some(
                "valkey is reached without tls on a host that is not loopback (development profile only)",
            )),
        }
    }
}
