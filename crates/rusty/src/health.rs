//! `rusty healthcheck`: the container HEALTHCHECK. It reads the same configuration as the daemon
//! (`--config FILE`, default `/etc/rusty/rusty.toml`, or `--dev ...`) and probes every enabled
//! listener on the loopback address: a TCP connect for each, and for the plain-text listeners
//! (25, 587, 143, 110) the greeting line (`220 ` for SMTP, `* OK` for IMAP, `+OK` for POP3). Implicit-TLS listeners
//! (465, 993, 995) are connect-only, so the probe needs no TLS stack, no credentials and creates no
//! session state. Exit code 0 when every listener answers, 1 otherwise.
//!
//! Behind the edge (`proxy_protocol`) a service refuses a connection that does not start with a PROXY header from a trusted address, and its implicit-TLS ports speak
//! plain text (TLS ended at the edge). The probe then sends a PROXY v2 `LOCAL` header first (what a proxy's own health check sends) and reads the greeting on every
//! listener, the implicit-TLS ones included. The probe connects from loopback, so `127.0.0.1/32` has to be in `trusted_proxies`: only a process inside the container's own
//! network namespace can connect from there.

use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use rusty_core::config::{Role, StartupConfig};
use rusty_svc::{listener_plan_for, ListenerKind};

const TIMEOUT: Duration = Duration::from_secs(3);

/// The address a probe connects to: a wildcard bind is probed on loopback.
fn probe_addr(bound: SocketAddr) -> SocketAddr {
    let ip = match bound.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    SocketAddr::new(ip, bound.port())
}

fn expected_greeting(kind: ListenerKind, proxied: bool) -> Option<&'static str> {
    match kind {
        ListenerKind::Smtp25 | ListenerKind::Submission587 => Some("220 "),
        ListenerKind::Imap143 => Some("* OK"),
        ListenerKind::Pop3110 => Some("+OK"),
        // TLS ends at the edge when proxied: the service itself speaks plain text on these ports.
        ListenerKind::Submissions465 => proxied.then_some("220 "),
        ListenerKind::Imaps993 => proxied.then_some("* OK"),
        ListenerKind::Pop3s995 => proxied.then_some("+OK"),
    }
}

/// Probe one listener at `bound`; `proxied` when the service expects a PROXY header first (see the module docs).
pub fn probe(kind: ListenerKind, bound: SocketAddr, proxied: bool) -> Result<(), String> {
    let addr = probe_addr(bound);
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("{}: connect {addr}: {e}", kind.name()))?;
    if proxied {
        // PROXY v2, command LOCAL, no address family, no payload: the proxy's own health check.
        let mut header = rusty_svc::proxy::SIGNATURE.to_vec();
        header.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
        std::io::Write::write_all(&mut stream, &header).map_err(|e| format!("{}: {e}", kind.name()))?;
    }
    let Some(want) = expected_greeting(kind, proxied) else {
        return Ok(());
    };
    stream.set_read_timeout(Some(TIMEOUT)).map_err(|e| format!("{}: {e}", kind.name()))?;
    let mut line = String::new();
    // Bounded read: a greeting is one short line.
    let mut reader = BufReader::new(std::io::Read::take(&stream, 512));
    reader.read_line(&mut line).map_err(|e| format!("{}: no greeting from {addr}: {e}", kind.name()))?;
    if line.starts_with(want) {
        Ok(())
    } else {
        Err(format!("{}: unexpected greeting from {addr}", kind.name()))
    }
}

/// Probe every enabled listener of `cfg`; the error lists every failing one.
pub fn check(cfg: &StartupConfig) -> Result<usize, String> {
    check_role(cfg, None)
}

/// [`check`] for one service: its listeners only. A service with no port (outbound, gc) has nothing to probe: its configuration loaded, which is all a probe from outside the
/// process can tell; `Ok(0)`.
pub fn check_role(cfg: &StartupConfig, role: Option<Role>) -> Result<usize, String> {
    let plan = listener_plan_for(cfg, role);
    if plan.is_empty() {
        return if matches!(role, Some(Role::Outbound | Role::Gc)) { Ok(0) } else { Err("no listener is enabled".to_owned()) };
    }
    let proxied = |kind: ListenerKind| if kind.is_imap() { cfg.imap.proxy_protocol } else { cfg.smtp.proxy_protocol };
    let failures: Vec<String> = plan.iter().filter_map(|p| probe(p.kind, p.spec.addr, proxied(p.kind)).err()).collect();
    if failures.is_empty() {
        Ok(plan.len())
    } else {
        Err(failures.join("; "))
    }
}
