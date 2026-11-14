//! Service sections of the startup configuration (U1): `[tls]`, `[smtp]`, `[imap]`,
//! `[outbound]`, `[limits]`, plus the listener address type. All keys optional with safe
//! defaults; unknown keys are rejected. Error texts never echo values.

use std::collections::BTreeMap;
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

/// How a destination domain's SMTP server is treated for TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TlsPolicy {
    /// No STARTTLS (plain text only). For test networks and legacy peers.
    None,
    /// STARTTLS when offered, certificate NOT verified (the Internet default for MTA-to-MTA mail),
    /// plain text when not offered or when the handshake fails.
    #[default]
    Opportunistic,
    /// TLS is mandatory (a missing STARTTLS or a failed handshake defers the mail), certificate
    /// not verified.
    Required,
    /// TLS is mandatory and the certificate chain and host name are verified against the
    /// configured `ca_bundle` (or the built-in web roots).
    Verified,
}

/// `[outbound.relay]`: send all remote mail through a smarthost instead of the recipients' MX.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub tls: RelayTls,
    pub username: Option<String>,
    /// File holding the password (mode 0600, read through `SecretSource`); never inline.
    pub password_file: Option<PathBuf>,
    /// The provider's SPF include (`amazonses.com`, `_spf.example.org`): the generated SPF record names it, because mail
    /// then leaves from the provider's addresses, not ours.
    #[serde(default)]
    pub spf_include: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RelayTls {
    #[default]
    Starttls,
    Implicit,
    None,
}

/// `[outbound]`: the queue worker, MX delivery, retry schedule, TLS policy and DKIM.
///
/// `nameserver`, `ca_bundle` and `bind_address` are deployment settings of the operator, read only
/// from this file. No domain, account, message or remote server can set or change them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundConfig {
    /// A message that is still undelivered after this long is bounced.
    #[serde(default = "d_max_age")]
    pub max_age_secs: u64,
    /// Delay before retry 1, 2, ...; the last entry repeats until `max_age_secs`.
    #[serde(default = "d_backoff")]
    pub backoff_secs: Vec<u32>,
    /// Simultaneous deliveries to one destination domain.
    #[serde(default = "d_per_domain")]
    pub max_concurrent_per_domain: u32,
    /// Simultaneous deliveries in all.
    #[serde(default = "d_concurrent")]
    pub max_concurrent: u32,
    #[serde(default = "d_connect")]
    pub connect_timeout_secs: u64,
    /// Per command/reply round trip.
    #[serde(default = "d_command")]
    pub command_timeout_secs: u64,
    /// For the DATA transfer and the reply to it.
    #[serde(default = "d_data")]
    pub data_timeout_secs: u64,
    /// Job lease; renewed by a heartbeat every third of it.
    #[serde(default = "d_lease")]
    pub lease_secs: u32,
    /// Name given in EHLO; defaults to `hostname`.
    pub helo_name: Option<String>,
    /// Local address outbound connections bind to (dedicated sending IP).
    pub bind_address: Option<IpAddr>,
    /// DNS server used for MX/A lookups instead of the system resolver.
    pub nameserver: Option<SocketAddr>,
    /// PEM bundle of CA certificates used for `verified` TLS instead of the built-in web roots.
    pub ca_bundle: Option<PathBuf>,
    #[serde(default)]
    pub default_tls: TlsPolicy,
    /// Per destination domain (lower-case ASCII), overriding `default_tls`.
    #[serde(default)]
    pub tls_policy: BTreeMap<String, TlsPolicy>,
    /// Directory of DKIM private keys `<domain>.<selector>.pem` (mode 0600). Unset: no signing.
    pub dkim_dir: Option<PathBuf>,
    #[serde(default = "d_selector")]
    pub dkim_selector: String,
    /// Sign with the ed25519 key (`<domain>.<selector>-ed25519.pem`, RFC 8463) when it exists, besides the RSA
    /// key when that exists. Default on: `rustyctl dkim generate` makes ed25519 keys. `false` ignores the file.
    #[serde(default = "d_true")]
    pub dkim_ed25519: bool,
    pub relay: Option<RelayConfig>,
    /// The public addresses mail leaves from (and the mail host resolves to): what the DNS page and `rustyctl domain check`
    /// write into the SPF and the address records. At most 8, each a unicast address that can be published.
    #[serde(default)]
    pub public_ips: Vec<IpAddr>,
    /// Run the outbound worker and accept mail for remote recipients on submission. When `false`,
    /// submission refuses remote recipients with a temporary status (it never accepts mail that no
    /// worker will send).
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Most queued or running jobs one account may have (fairness inside a tenant).
    #[serde(default = "d_pending_account")]
    pub max_pending_per_account: u32,
    /// The window of the submission budget below.
    #[serde(default = "d_window")]
    pub submit_window_secs: u64,
    /// Per account and window: jobs (one per destination domain), recipients, message bytes.
    #[serde(default = "d_jobs")]
    pub submit_max_jobs: u32,
    #[serde(default = "d_rcpts")]
    pub submit_max_recipients: u64,
    #[serde(default = "d_bytes")]
    pub submit_max_bytes: u64,
}

fn d_true() -> bool {
    true
}
fn d_pending_account() -> u32 {
    1000
}
fn d_window() -> u64 {
    3600
}
fn d_jobs() -> u32 {
    500
}
fn d_rcpts() -> u64 {
    2000
}
fn d_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

fn d_max_age() -> u64 {
    5 * 86_400
}
fn d_backoff() -> Vec<u32> {
    vec![300, 900, 1800, 3600, 7200, 14_400]
}
fn d_per_domain() -> u32 {
    5
}
fn d_concurrent() -> u32 {
    20
}
fn d_connect() -> u64 {
    30
}
fn d_command() -> u64 {
    120
}
fn d_data() -> u64 {
    600
}
fn d_lease() -> u32 {
    1800
}
fn d_selector() -> String {
    "rusty".into()
}

impl Default for OutboundConfig {
    fn default() -> Self {
        OutboundConfig {
            max_age_secs: d_max_age(),
            backoff_secs: d_backoff(),
            max_concurrent_per_domain: d_per_domain(),
            max_concurrent: d_concurrent(),
            connect_timeout_secs: d_connect(),
            command_timeout_secs: d_command(),
            data_timeout_secs: d_data(),
            lease_secs: d_lease(),
            helo_name: None,
            bind_address: None,
            nameserver: None,
            ca_bundle: None,
            default_tls: TlsPolicy::default(),
            tls_policy: BTreeMap::new(),
            dkim_dir: None,
            dkim_selector: d_selector(),
            dkim_ed25519: true,
            relay: None,
            public_ips: Vec::new(),
            enabled: true,
            max_pending_per_account: d_pending_account(),
            submit_window_secs: d_window(),
            submit_max_jobs: d_jobs(),
            submit_max_recipients: d_rcpts(),
            submit_max_bytes: d_bytes(),
        }
    }
}

impl OutboundConfig {
    /// The TLS policy for mail to `domain` (lower-case).
    pub fn tls_for(&self, domain: &str) -> TlsPolicy {
        self.tls_policy.get(domain).copied().unwrap_or(self.default_tls)
    }

    /// Delay before retry number `retry` (0-based): the schedule, then its last entry forever.
    pub fn backoff_for(&self, retry: u32) -> u32 {
        let i = usize::try_from(retry).unwrap_or(usize::MAX);
        self.backoff_secs.get(i).or_else(|| self.backoff_secs.last()).copied().unwrap_or(14_400)
    }

    pub(crate) fn validate(&mut self) -> Result<()> {
        if !(3600..=30 * 86_400).contains(&self.max_age_secs) {
            return Err(bad("outbound.max_age_secs must be 3600..=2592000"));
        }
        if self.backoff_secs.is_empty() || self.backoff_secs.len() > 64 || self.backoff_secs.iter().any(|s| !(1..=86_400).contains(s)) {
            return Err(bad("outbound.backoff_secs needs 1..=64 entries of 1..=86400"));
        }
        if self.max_pending_per_account == 0 || !(60..=7 * 86_400).contains(&self.submit_window_secs) || self.submit_max_jobs == 0 || self.submit_max_recipients == 0 || self.submit_max_bytes == 0 {
            return Err(bad("outbound submission limits are out of range"));
        }
        if !(1..=1000).contains(&self.max_concurrent_per_domain) || !(1..=10_000).contains(&self.max_concurrent) {
            return Err(bad("outbound concurrency limits are out of range"));
        }
        for t in [self.connect_timeout_secs, self.command_timeout_secs, self.data_timeout_secs] {
            if !(1..=3600).contains(&t) {
                return Err(bad("outbound timeouts must be 1..=3600 seconds"));
            }
        }
        if !(60..=86_400).contains(&self.lease_secs)
            || u64::from(self.lease_secs) < self.data_timeout_secs + self.command_timeout_secs
        {
            return Err(bad("outbound.lease_secs must be 60..=86400 and cover the data and command timeouts"));
        }
        if let Some(h) = &self.helo_name {
            if !valid_hostname(h) {
                return Err(bad("outbound.helo_name is not a valid host name"));
            }
        }
        for p in [&self.ca_bundle, &self.dkim_dir].into_iter().flatten() {
            if !p.is_absolute() {
                return Err(bad("outbound paths must be absolute"));
            }
        }
        if self.dkim_selector.is_empty()
            || self.dkim_selector.len() > 63
            || !self.dkim_selector.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(bad("outbound.dkim_selector must be letters, digits and hyphens"));
        }
        let mut norm = BTreeMap::new();
        for (d, p) in std::mem::take(&mut self.tls_policy) {
            let d = d.to_ascii_lowercase();
            if !valid_hostname(&d) {
                return Err(bad("outbound.tls_policy keys must be domain names"));
            }
            norm.insert(d, p);
        }
        self.tls_policy = norm;
        if self.public_ips.len() > 8 {
            return Err(bad("outbound.public_ips has at most 8 addresses"));
        }
        for (n, ip) in self.public_ips.iter().enumerate() {
            if !crate::dns::publishable_address(*ip) || self.public_ips.iter().take(n).any(|o| o == ip) {
                return Err(bad("outbound.public_ips must be distinct unicast addresses that can be published"));
            }
        }
        if let Some(r) = &self.relay {
            if !valid_hostname(&r.host) && r.host.parse::<IpAddr>().is_err() {
                return Err(bad("outbound.relay.host is not a host name or address"));
            }
            if r.port == 0 {
                return Err(bad("outbound.relay.port must be set"));
            }
            if r.tls == RelayTls::None && r.username.is_some() {
                return Err(bad("outbound.relay credentials need TLS (tls = starttls or implicit)"));
            }
            if r.spf_include.as_deref().is_some_and(|d| !crate::dns::valid_spf_domain(d)) {
                return Err(bad("outbound.relay.spf_include must be a domain name (no \"include:\", no trailing dot)"));
            }
            match (&r.username, &r.password_file) {
                (None, None) => {}
                (Some(u), Some(p)) => {
                    if u.is_empty() || u.len() > 255 || !p.is_absolute() {
                        return Err(bad("outbound.relay credentials are invalid (password_file must be absolute)"));
                    }
                }
                _ => return Err(bad("outbound.relay.username and password_file must be set together")),
            }
        }
        Ok(())
    }
}

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
    /// Memory (MiB) all SMTP sessions together may hold for messages being received and processed.
    /// A message in flight is counted about four times (received, normalised, prepared, handed to the
    /// store). When the budget is used up new data is refused with a temporary status.
    #[serde(default = "d_inflight_mib")]
    pub inflight_message_mib: u32,
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
fn d_inflight_mib() -> u32 {
    1024
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
            inflight_message_mib: d_inflight_mib(),
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
        if !(16..=1_048_576).contains(&self.inflight_message_mib) {
            return Err(bad("limits.inflight_message_mib must be 16..=1048576"));
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

/// A resolver address: `IP` (port 53) or `IP:port` / `[v6]:port`. A host NAME is refused on
/// purpose: resolving the resolver would need a resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nameserver(SocketAddr);

impl Nameserver {
    pub fn addr(&self) -> SocketAddr {
        self.0
    }
}


impl<'de> Deserialize<'de> for Nameserver {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Nameserver, D::Error> {
        let s = String::deserialize(d)?;
        let parsed = s
            .parse::<SocketAddr>()
            .ok()
            .or_else(|| s.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 53)))
            .filter(|a| a.port() != 0)
            .ok_or_else(|| serde::de::Error::custom("nameserver must be an IP address, optionally with a port"))?;
        Ok(Nameserver(parsed))
    }
}

/// `[dns_check]` (U10 part 1b): the periodic `dns.check` job that re-validates every active
/// domain's DNS records and records status changes. Off by default. The seam for the daemon
/// wiring: job kind [`DNS_CHECK_JOB_KIND`], this section, and `rusty_dnscheck::run_dns_check`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsCheckConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Time between two checks of one domain.
    #[serde(default = "d_dns_interval")]
    pub interval_secs: u64,
    /// Each domain's check time is spread over this many seconds after the interval (a stable
    /// per-domain offset), so domains are not all queried in the same minute.
    #[serde(default = "d_dns_jitter")]
    pub jitter_secs: u64,
    /// Consecutive failures of a record that was `ok` before an alert is raised.
    #[serde(default = "d_dns_alert_after")]
    pub alert_after_failures: u32,
    /// Resolver to use instead of the system one (`IP` or `IP:port`).
    #[serde(default)]
    pub nameserver: Option<Nameserver>,
    /// Second, independent resolver for the two-resolver comparison (propagation lag shows as
    /// `pending`, not `mismatch`). Needs `nameserver`; without it the comparison is skipped and
    /// the report says so.
    #[serde(default)]
    pub second_nameserver: Option<Nameserver>,
}

/// The job kind of the periodic check (see [`DnsCheckConfig`]).
pub const DNS_CHECK_JOB_KIND: &str = "dns.check";

fn d_dns_interval() -> u64 {
    86_400
}
fn d_dns_jitter() -> u64 {
    3_600
}
fn d_dns_alert_after() -> u32 {
    3
}

impl Default for DnsCheckConfig {
    fn default() -> Self {
        DnsCheckConfig {
            enabled: false,
            interval_secs: d_dns_interval(),
            jitter_secs: d_dns_jitter(),
            alert_after_failures: d_dns_alert_after(),
            nameserver: None,
            second_nameserver: None,
        }
    }
}

impl DnsCheckConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(3_600..=2_592_000).contains(&self.interval_secs) {
            // Not below an hour: the job queries other people's name servers.
            return Err(bad("dns_check.interval_secs must be 3600..=2592000"));
        }
        if self.jitter_secs > self.interval_secs {
            return Err(bad("dns_check.jitter_secs must not exceed interval_secs"));
        }
        if !(1..=100).contains(&self.alert_after_failures) {
            return Err(bad("dns_check.alert_after_failures must be 1..=100"));
        }
        match (self.nameserver, self.second_nameserver) {
            (None, Some(_)) => Err(bad("dns_check.second_nameserver needs dns_check.nameserver")),
            (Some(a), Some(b)) if a == b => Err(bad("dns_check.nameserver and second_nameserver must differ")),
            _ => Ok(()),
        }
    }
}
