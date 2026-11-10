//! `rusty healthcheck`: the container HEALTHCHECK. It reads the same configuration as the daemon
//! (`--config FILE`, default `/etc/rusty/rusty.toml`, or `--dev ...`) and probes every enabled
//! listener on the loopback address: a TCP connect for each, and for the plain-text listeners
//! (25, 587, 143) the greeting line (`220 ` for SMTP, `* OK` for IMAP). Implicit-TLS listeners
//! (465, 993) are connect-only, so the probe needs no TLS stack, no credentials and creates no
//! session state. Exit code 0 when every listener answers, 1 otherwise.

use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_svc::{listener_plan, ListenerKind};

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

fn expected_greeting(kind: ListenerKind) -> Option<&'static str> {
    match kind {
        ListenerKind::Smtp25 | ListenerKind::Submission587 => Some("220 "),
        ListenerKind::Imap143 => Some("* OK"),
        ListenerKind::Submissions465 | ListenerKind::Imaps993 => None,
    }
}

fn probe(kind: ListenerKind, bound: SocketAddr) -> Result<(), String> {
    let addr = probe_addr(bound);
    let stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("{}: connect {addr}: {e}", kind.name()))?;
    let Some(want) = expected_greeting(kind) else {
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
    let plan = listener_plan(cfg);
    if plan.is_empty() {
        return Err("no listener is enabled".to_owned());
    }
    let failures: Vec<String> = plan.iter().filter_map(|p| probe(p.kind, p.spec.addr).err()).collect();
    if failures.is_empty() {
        Ok(plan.len())
    } else {
        Err(failures.join("; "))
    }
}
