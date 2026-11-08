//! Service sections of the startup configuration (U1): `[tls]`, `[smtp]`, `[imap]`,
//! `[outbound]`, `[limits]`, plus the listener address type. All keys optional with safe
//! defaults; unknown keys are rejected. Error texts never echo values.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

use crate::{Error, Result};

/// A listener address. The empty string in the file disables the listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listen(Option<SocketAddr>);

impl Listen {
    pub const fn on(addr: SocketAddr) -> Listen {
        Listen(Some(addr))
    }
    pub const fn off() -> Listen {
        Listen(None)
    }
    pub fn addr(&self) -> Option<SocketAddr> {
        self.0
    }
    const fn any(port: u16) -> Listen {
        Listen(Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)))
    }
    pub(crate) const fn loopback(port: u16) -> Listen {
        Listen(Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)))
    }
}

impl<'de> Deserialize<'de> for Listen {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Listen, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(Listen::off());
        }
        s.parse::<SocketAddr>()
            .map(Listen::on)
            .map_err(|_| serde::de::Error::custom("listener address must be host:port"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// `[tls]`: one certificate/key pair (SAN covers the mail hostname), or the explicit dev cert.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_file: Option<PathBuf>,
    /// Must be a regular file with mode 0600 (no group/world access); refused otherwise.
    pub key_file: Option<PathBuf>,
    /// Generate an in-memory self-signed certificate (needs the `dev-cert` cargo feature).
    /// Loud warning in the log. Never implied by anything but `rusty d --dev`.
    #[serde(default)]
    pub dev_self_signed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    #[serde(default = "l25")]
    pub listen_25: Listen,
    #[serde(default = "l587")]
    pub listen_587: Listen,
    #[serde(default = "l465")]
    pub listen_465: Listen,
    /// Override of the top-level cap for SMTP; may not exceed it.
    pub max_message_bytes: Option<u64>,
    #[serde(default = "d_max_recipients")]
    pub max_recipients: u32,
    #[serde(default = "d_max_connections")]
    pub max_connections: u32,
    #[serde(default = "d_per_ip_smtp")]
    pub max_connections_per_ip: u32,
    /// Seam for an edge proxy (PROXY protocol); `true` is rejected until it is implemented.
    #[serde(default)]
    pub proxy_protocol: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImapConfig {
    #[serde(default = "l143")]
    pub listen_143: Listen,
    #[serde(default = "l993")]
    pub listen_993: Listen,
    #[serde(default = "d_max_connections")]
    pub max_connections: u32,
    #[serde(default = "d_per_ip_imap")]
    pub max_connections_per_ip: u32,
    #[serde(default)]
    pub proxy_protocol: bool,
}

/// `[outbound]`: filled by U5; accepts no keys yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OutboundConfig {}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// How long a shutdown waits for sessions to drain before it cuts them.
    #[serde(default = "d_grace")]
    pub shutdown_grace_secs: u64,
    /// Authentication failures per (peer IP, identity) inside the window before throttling.
    #[serde(default = "d_auth_failures")]
    pub auth_failures_max: u32,
    #[serde(default = "d_auth_window")]
    pub auth_window_secs: u64,
    /// Authentication attempts allowed to run at the same time, process-wide.
    #[serde(default = "d_auth_concurrent")]
    pub auth_concurrent_max: u32,
}

fn l25() -> Listen {
    Listen::any(25)
}
fn l587() -> Listen {
    Listen::any(587)
}
fn l465() -> Listen {
    Listen::any(465)
}
fn l143() -> Listen {
    Listen::any(143)
}
fn l993() -> Listen {
    Listen::any(993)
}
fn d_max_recipients() -> u32 {
    100
}
fn d_max_connections() -> u32 {
    1000
}
fn d_per_ip_smtp() -> u32 {
    20
}
fn d_per_ip_imap() -> u32 {
    50
}
fn d_grace() -> u64 {
    30
}
fn d_auth_failures() -> u32 {
    5
}
fn d_auth_window() -> u64 {
    300
}
fn d_auth_concurrent() -> u32 {
    16
}

impl Default for SmtpConfig {
    fn default() -> Self {
        SmtpConfig {
            listen_25: l25(),
            listen_587: l587(),
            listen_465: l465(),
            max_message_bytes: None,
            max_recipients: d_max_recipients(),
            max_connections: d_max_connections(),
            max_connections_per_ip: d_per_ip_smtp(),
            proxy_protocol: false,
        }
    }
}

impl Default for ImapConfig {
    fn default() -> Self {
        ImapConfig {
            listen_143: l143(),
            listen_993: l993(),
            max_connections: d_max_connections(),
            max_connections_per_ip: d_per_ip_imap(),
            proxy_protocol: false,
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            shutdown_grace_secs: d_grace(),
            auth_failures_max: d_auth_failures(),
            auth_window_secs: d_auth_window(),
            auth_concurrent_max: d_auth_concurrent(),
        }
    }
}

fn bad(msg: &str) -> Error {
    Error::Invalid(msg.into())
}

impl TlsConfig {
    /// A certificate and key file are configured (the dev cert does not count).
    pub fn has_cert(&self) -> bool {
        self.cert_file.is_some() && self.key_file.is_some()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        match (&self.cert_file, &self.key_file) {
            (None, None) => {}
            (Some(c), Some(k)) => {
                if !c.is_absolute() || !k.is_absolute() {
                    return Err(bad("tls.cert_file and tls.key_file must be absolute"));
                }
                if self.dev_self_signed {
                    return Err(bad("tls.dev_self_signed cannot be combined with a certificate"));
                }
            }
            _ => return Err(bad("tls.cert_file and tls.key_file must be set together")),
        }
        Ok(())
    }
}

impl SmtpConfig {
    pub(crate) fn validate(&self, global_max_message: u64) -> Result<()> {
        if let Some(m) = self.max_message_bytes {
            if !(crate::config::MIN_MESSAGE_BYTES..=global_max_message).contains(&m) {
                return Err(bad("smtp.max_message_bytes must be 1 KiB..=max_message_bytes"));
            }
        }
        if !(1..=10_000).contains(&self.max_recipients) {
            return Err(bad("smtp.max_recipients must be 1..=10000"));
        }
        if self.max_connections == 0 || self.max_connections_per_ip == 0 {
            return Err(bad("smtp connection limits must be at least 1"));
        }
        if self.proxy_protocol {
            return Err(bad("smtp.proxy_protocol is not implemented yet"));
        }
        Ok(())
    }
}

impl ImapConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.max_connections == 0 || self.max_connections_per_ip == 0 {
            return Err(bad("imap connection limits must be at least 1"));
        }
        if self.proxy_protocol {
            return Err(bad("imap.proxy_protocol is not implemented yet"));
        }
        Ok(())
    }
}

impl LimitsConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=600).contains(&self.shutdown_grace_secs) {
            return Err(bad("limits.shutdown_grace_secs must be 1..=600"));
        }
        if self.auth_failures_max == 0 || self.auth_concurrent_max == 0 {
            return Err(bad("limits.auth_* counts must be at least 1"));
        }
        if !(1..=86_400).contains(&self.auth_window_secs) {
            return Err(bad("limits.auth_window_secs must be 1..=86400"));
        }
        Ok(())
    }
}

/// A DNS host name as it may appear in `hostname`: ASCII letters, digits, hyphen and dots,
/// labels of 1..=63 not starting or ending with a hyphen, at most 253 bytes in all.
pub fn valid_hostname(h: &str) -> bool {
    let h = h.strip_suffix('.').unwrap_or(h);
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
