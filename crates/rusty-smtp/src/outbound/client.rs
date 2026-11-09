//! The SMTP client: ONE delivery session over a stream the caller connected. It speaks the protocol
//! (greeting, EHLO, optional STARTTLS, optional AUTH for a relay, MAIL, RCPT per recipient, DATA) and
//! reports a result PER RECIPIENT, which is what partial success and bounces need.
//!
//! Layering (plan section 1b): reply parsing is `smtp_reply` (pure), TLS is the injected
//! [`TlsUpgrader`], connecting and DNS happen outside (the worker), and the decisions about retry or
//! bounce are `policy`. This file is only the conversation. Nothing here reads a clock except through
//! tokio timeouts.
//!
//! Failure semantics:
//! * a connection-level failure (greeting, EHLO, TLS, MAIL, a lost connection) is a [`HostFailure`]:
//!   the worker tries the next exchanger, and when none works defers or bounces by the failure kind;
//! * recipient-level replies (RCPT 4xx/5xx, the answer after DATA) become per-recipient outcomes;
//! * a connection lost AFTER the message was sent but before the final reply is `Temporary`: the
//!   remote may or may not have accepted it, so the retry can deliver it twice (SMTP is at-least-once;
//!   documented in the module docs of `worker`).

use std::time::Duration;

use rusty_core::config::TlsPolicy;
use rusty_svc::BoxedStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::policy::{classify, Outcome};
use super::smtp_reply::{parse_reply, Class, Reply};
use super::tls::{TlsInfo, TlsUpgrader};

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// One command and its reply.
    pub command: Duration,
    /// Sending the message and receiving the reply to its end.
    pub data: Duration,
}

pub struct SessionParams<'a> {
    /// Name given in EHLO.
    pub helo: &'a str,
    /// Name the certificate must match (strict TLS) and the TLS SNI.
    pub server_name: &'a str,
    pub policy: TlsPolicy,
    /// TLS from the first byte (port 465 style relays).
    pub implicit_tls: bool,
    /// Relay credentials; only ever sent over TLS.
    pub auth: Option<(&'a str, &'a str)>,
    pub timeouts: Timeouts,
}

pub struct Mail<'a> {
    /// Empty: null sender.
    pub from: &'a str,
    pub recipients: &'a [String],
    pub data: &'a [u8],
}

/// Asked right before the message is sent: the last moment to find out the job lease was lost.
#[async_trait::async_trait]
pub trait Gate: Send + Sync {
    async fn proceed(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostFailure {
    /// Retry later (or try another exchanger).
    Temporary(String),
    /// The server refuses us for good (5xx greeting, MAIL FROM, DATA).
    Permanent { diagnostic: String, status: String },
    /// The STARTTLS handshake failed; a plain-text retry is allowed when the policy says opportunistic.
    TlsHandshake(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcptResult {
    pub recipient: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub results: Vec<RcptResult>,
    /// The host name in the server's greeting (for `Remote-MTA`).
    pub remote: String,
    pub tls: Option<TlsInfo>,
}

#[derive(Default, Debug)]
struct Caps {
    starttls: bool,
    size: Option<u64>,
    eightbit: bool,
    smtputf8: bool,
    auth_plain: bool,
}

fn parse_caps(r: &Reply) -> Caps {
    let mut c = Caps::default();
    for line in r.lines.iter().skip(1) {
        let mut it = line.split_whitespace();
        let kw = it.next().unwrap_or_default().to_ascii_uppercase();
        match kw.as_str() {
            "STARTTLS" => c.starttls = true,
            "8BITMIME" => c.eightbit = true,
            "SMTPUTF8" => c.smtputf8 = true,
            "SIZE" => c.size = it.next().and_then(|n| n.parse().ok()),
            "AUTH" => c.auth_plain = it.any(|m| m.eq_ignore_ascii_case("PLAIN")),
            _ => {}
        }
    }
    c
}

struct Conn {
    stream: BoxedStream,
    buf: Vec<u8>,
}

fn temp(m: impl Into<String>) -> HostFailure {
    HostFailure::Temporary(m.into())
}

impl Conn {
    async fn read_reply(&mut self, limit: Duration) -> Result<Reply, HostFailure> {
        loop {
            match parse_reply(&self.buf) {
                Ok(Some((reply, used))) => {
                    self.buf.drain(..used);
                    return Ok(reply);
                }
                Ok(None) => {}
                Err(_) => return Err(temp("the server sent an invalid reply")),
            }
            let mut chunk = [0u8; 4096];
            match timeout(limit, self.stream.read(&mut chunk)).await {
                Err(_) => return Err(temp("timed out waiting for the server")),
                Ok(Err(e)) => return Err(temp(format!("connection error: {}", e.kind()))),
                Ok(Ok(0)) => return Err(temp("the server closed the connection")),
                Ok(Ok(n)) => self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            }
        }
    }

    async fn send(&mut self, bytes: &[u8], limit: Duration) -> Result<(), HostFailure> {
        match timeout(limit, async {
            self.stream.write_all(bytes).await?;
            self.stream.flush().await
        })
        .await
        {
            Err(_) => Err(temp("timed out sending to the server")),
            Ok(Err(e)) => Err(temp(format!("connection error: {}", e.kind()))),
            Ok(Ok(())) => Ok(()),
        }
    }

    async fn command(&mut self, line: &str, limit: Duration) -> Result<Reply, HostFailure> {
        self.send(format!("{line}\r\n").as_bytes(), limit).await?;
        self.read_reply(limit).await
    }
}

/// Domain to A-label, local part kept; `(address on the wire, needs SMTPUTF8)`.
fn wire_address(a: &str) -> Option<(String, bool)> {
    let (local, domain) = a.rsplit_once('@')?;
    let domain = crate::resolve::to_ascii_domain(domain)?;
    Some((format!("{local}@{domain}"), !local.is_ascii()))
}

/// Dot-stuff `data` and terminate it: every line ends CRLF (bare LF and bare CR repaired), a line
/// starting with `.` gets another one, and `.CRLF` ends the data.
pub fn encode_data(data: &[u8]) -> Vec<u8> {
    let norm = crate::message::normalise_crlf(data);
    let mut out = Vec::with_capacity(norm.len() + norm.len() / 64 + 8);
    let mut line_start = true;
    for &b in &norm {
        if line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        line_start = b == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

fn permanent(r: &Reply) -> HostFailure {
    HostFailure::Permanent { diagnostic: r.summary(), status: r.enhanced.clone().unwrap_or_else(|| "5.0.0".into()) }
}

fn refused(r: &Reply) -> HostFailure {
    match r.class() {
        Class::Permanent => permanent(r),
        _ => HostFailure::Temporary(r.summary()),
    }
}

/// Run one delivery session. See the module docs for the failure semantics.
pub async fn run_session(
    stream: BoxedStream,
    p: &SessionParams<'_>,
    tls: &dyn TlsUpgrader,
    mail: &Mail<'_>,
    gate: &dyn Gate,
    cancel: &CancellationToken,
) -> Result<Delivery, HostFailure> {
    let mut tls_info: Option<TlsInfo> = None;
    let strict = p.policy == TlsPolicy::Verified;
    let mut conn = Conn { stream, buf: Vec::new() };
    if p.implicit_tls {
        let (s, info) = tls.upgrade(conn.stream, p.server_name, strict).await.map_err(HostFailure::TlsHandshake)?;
        conn = Conn { stream: s, buf: Vec::new() };
        tls_info = Some(info);
    }

    // Greeting.
    let greeting = conn.read_reply(p.timeouts.command).await?;
    match greeting.class() {
        Class::Positive => {}
        Class::Permanent => return Err(permanent(&greeting)),
        _ => return Err(HostFailure::Temporary(greeting.summary())),
    }
    let remote = greeting.lines.first().and_then(|l| l.split_whitespace().next()).unwrap_or_default().chars().take(253).collect::<String>();

    // EHLO, with the HELO fallback for servers that do not know it.
    let mut caps = ehlo(&mut conn, p).await?;

    // STARTTLS by policy.
    if tls_info.is_none() && p.policy != TlsPolicy::None {
        if caps.starttls {
            let r = conn.command("STARTTLS", p.timeouts.command).await?;
            if r.class() != Class::Positive {
                if p.policy == TlsPolicy::Opportunistic {
                    // The server advertised it and then refused: carry on in clear, as the policy allows.
                } else {
                    return Err(HostFailure::Temporary(format!("STARTTLS refused: {}", r.summary())));
                }
            } else {
                if !conn.buf.is_empty() {
                    return Err(HostFailure::TlsHandshake("the server sent data before the TLS handshake".into()));
                }
                let (s, info) = tls.upgrade(conn.stream, p.server_name, strict).await.map_err(HostFailure::TlsHandshake)?;
                conn = Conn { stream: s, buf: Vec::new() };
                tls_info = Some(info);
                caps = ehlo(&mut conn, p).await?;
            }
        } else if p.policy != TlsPolicy::Opportunistic {
            return Err(HostFailure::Temporary("TLS is required for this destination but the server does not offer STARTTLS".into()));
        }
    }
    if tls_info.is_none() && p.policy != TlsPolicy::None && p.policy != TlsPolicy::Opportunistic {
        return Err(HostFailure::Temporary("TLS is required for this destination".into()));
    }

    // Relay authentication, only over TLS (the password never crosses the network in clear).
    if let Some((user, pass)) = p.auth {
        if tls_info.is_none() {
            return Err(HostFailure::Temporary("refusing to send relay credentials without TLS".into()));
        }
        if !caps.auth_plain {
            return Err(HostFailure::Temporary("the relay does not offer AUTH PLAIN".into()));
        }
        use base64::Engine;
        let blob = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"));
        let r = conn.command(&format!("AUTH PLAIN {blob}"), p.timeouts.command).await?;
        if r.class() != Class::Positive {
            // Never echo the server's text next to credentials; a failed login is a configuration
            // problem, so defer (retrying is harmless) instead of bouncing the user's mail.
            return Err(HostFailure::Temporary("the relay refused the credentials".into()));
        }
    }

    // Envelope.
    let (from_wire, from_utf8) = if mail.from.is_empty() {
        (String::new(), false)
    } else {
        wire_address(mail.from).ok_or_else(|| HostFailure::Permanent { diagnostic: "invalid sender domain".into(), status: "5.1.7".into() })?
    };
    let eight = mail.data.iter().any(|b| *b >= 0x80);
    let mut params = String::new();
    if let Some(max) = caps.size {
        if max > 0 && mail.data.len() as u64 > max {
            return Err(HostFailure::Permanent { diagnostic: "message exceeds the remote server's size limit".into(), status: "5.3.4".into() });
        }
    }
    if caps.size.is_some() {
        params.push_str(&format!(" SIZE={}", mail.data.len()));
    }
    if eight {
        if !caps.eightbit {
            return Err(HostFailure::Permanent { diagnostic: "the message needs 8BITMIME and the remote server does not offer it".into(), status: "5.6.1".into() });
        }
        params.push_str(" BODY=8BITMIME");
    }
    let any_utf8 = from_utf8 || mail.recipients.iter().any(|r| wire_address(r).is_some_and(|(_, u)| u));
    if any_utf8 {
        if !caps.smtputf8 {
            // Only the addresses that need it fail (below); the sender needing it fails everything.
            if from_utf8 {
                return Err(HostFailure::Permanent { diagnostic: "the sender address needs SMTPUTF8 and the remote server does not offer it".into(), status: "5.6.7".into() });
            }
        } else {
            params.push_str(" SMTPUTF8");
        }
    }
    let r = conn.command(&format!("MAIL FROM:<{from_wire}>{params}"), p.timeouts.command).await?;
    if r.class() != Class::Positive {
        return Err(refused(&r));
    }

    let mut results: Vec<RcptResult> = Vec::with_capacity(mail.recipients.len());
    let mut accepted: Vec<usize> = Vec::new();
    for rcpt in mail.recipients {
        if cancel.is_cancelled() {
            return Err(temp("shutting down"));
        }
        let Some((wire, utf8)) = wire_address(rcpt) else {
            results.push(RcptResult {
                recipient: rcpt.clone(),
                outcome: Outcome::Permanent { diagnostic: "invalid recipient domain name".into(), status: "5.1.2".into() },
            });
            continue;
        };
        if utf8 && !caps.smtputf8 {
            results.push(RcptResult {
                recipient: rcpt.clone(),
                outcome: Outcome::Permanent {
                    diagnostic: "the recipient address needs SMTPUTF8 and the remote server does not offer it".into(),
                    status: "5.6.7".into(),
                },
            });
            continue;
        }
        let r = conn.command(&format!("RCPT TO:<{wire}>"), p.timeouts.command).await?;
        let outcome = classify(&r);
        if outcome == Outcome::Delivered {
            accepted.push(results.len());
        }
        results.push(RcptResult { recipient: rcpt.clone(), outcome });
    }

    if accepted.is_empty() {
        let _ = conn.command("QUIT", Duration::from_secs(5)).await;
        return Ok(Delivery { results, remote, tls: tls_info });
    }

    // The last chance to learn that someone else owns this job now.
    if !gate.proceed().await {
        let _ = conn.send(b"RSET\r\nQUIT\r\n", Duration::from_secs(5)).await;
        return Err(temp("the job lease was lost before the message was sent"));
    }

    let r = conn.command("DATA", p.timeouts.command).await?;
    if r.class() != Class::Intermediate {
        return Err(refused(&r));
    }
    let body = encode_data(mail.data);
    match timeout(p.timeouts.data, async {
        conn.send(&body, p.timeouts.data).await?;
        conn.read_reply(p.timeouts.data).await
    })
    .await
    {
        Err(_) => return Err(temp("timed out after sending the message; it may have been accepted")),
        Ok(Err(HostFailure::Temporary(m))) => return Err(temp(format!("{m} after the message was sent; it may have been accepted"))),
        Ok(Err(e)) => return Err(e),
        Ok(Ok(reply)) => {
            let outcome = classify(&reply);
            for i in accepted {
                if let Some(slot) = results.get_mut(i) {
                    slot.outcome = outcome.clone();
                }
            }
        }
    }
    let _ = conn.command("QUIT", Duration::from_secs(5)).await;
    Ok(Delivery { results, remote, tls: tls_info })
}

async fn ehlo(conn: &mut Conn, p: &SessionParams<'_>) -> Result<Caps, HostFailure> {
    let r = conn.command(&format!("EHLO {}", p.helo), p.timeouts.command).await?;
    match r.class() {
        Class::Positive => Ok(parse_caps(&r)),
        Class::Permanent => {
            // Servers that do not know EHLO get HELO and no extensions.
            let h = conn.command(&format!("HELO {}", p.helo), p.timeouts.command).await?;
            match h.class() {
                Class::Positive => Ok(Caps::default()),
                Class::Permanent => Err(permanent(&h)),
                _ => Err(HostFailure::Temporary(h.summary())),
            }
        }
        _ => Err(HostFailure::Temporary(r.summary())),
    }
}
