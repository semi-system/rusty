//! Inbound SPF, DKIM and DMARC verification as a Rusty-owned seam. The implementation sits behind
//! the DKIM/SPF library adapter; the SMTP driver only sees this trait. The result is LOG-ONLY: it fills the
//! `Authentication-Results` header and the log, it never rejects or quarantines (DMARC enforcement is
//! a deferred item).

use std::net::IpAddr;

use async_trait::async_trait;

/// What the checker needs from the session.
pub struct AuthInput<'a> {
    pub peer: IpAddr,
    /// The name the client gave in EHLO/HELO.
    pub helo: &'a str,
    /// Envelope sender; empty for the null sender.
    pub mail_from: &'a str,
    /// The message as received (before our trace headers).
    pub message: &'a [u8],
}

/// The verdicts, as the lower-case words of RFC 8601 (`pass`, `fail`, `none`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResults {
    /// The value of the `Authentication-Results` header after the field name:
    /// `<authserv-id>; spf=...; dkim=...; dmarc=...`, on one line.
    pub header_value: String,
    pub spf: String,
    /// The first signature's verdict (`none` without signatures).
    pub dkim: String,
    pub dmarc: String,
}

impl AuthResults {
    /// No checks done (the checker could not parse the message, or none is configured).
    pub fn none(hostname: &str) -> AuthResults {
        AuthResults { header_value: format!("{hostname}; none"), spf: "none".into(), dkim: "none".into(), dmarc: "none".into() }
    }
}

#[async_trait]
pub trait InboundAuthChecker: Send + Sync {
    async fn check(&self, input: &AuthInput<'_>) -> AuthResults;
}
