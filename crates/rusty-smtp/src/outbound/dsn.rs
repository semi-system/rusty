//! Delivery status notifications (bounces), RFC 3464: `multipart/report; report-type=delivery-status`
//! with a human-readable part, the machine-readable `message/delivery-status` part and the ORIGINAL
//! HEADERS ONLY of the failed message (`text/rfc822-headers`, truncated): never the body, so a bounce
//! cannot be used to exfiltrate or amplify content.
//!
//! No backscatter (plan section 4, item 8): a DSN is built only for a message a LOCAL authenticated
//! user submitted; the worker checks that the envelope sender resolves to an account of the sender's
//! tenant, and never writes one for the null sender. The bounce goes to that user's own Inbox, never to
//! the network, so a forged sender address can not be hit by it.
//!
//! Everything from the outside (remote diagnostics, the message's own headers) is sanitised: control
//! characters removed, lines bounded, and no line of copied headers may look like a MIME boundary.

use rusty_core::{BlobHash, DeliveryId};
use sha2::{Digest, Sha256};

use crate::message::rfc5322_date;

/// Longest header section copied into a bounce.
pub const MAX_HEADERS_BYTES: usize = 8 * 1024;
/// Longest diagnostic line.
const MAX_DIAG: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub recipient: String,
    /// RFC 3463 status, e.g. `5.1.1`; `4.4.7` for an expired message.
    pub status: String,
    pub diagnostic: String,
    /// The remote server that answered, when there was one.
    pub remote_mta: Option<String>,
}

pub struct DsnInput<'a> {
    /// Our host name (`Reporting-MTA`, the From domain).
    pub hostname: &'a str,
    /// The local user the bounce is for (the original envelope sender).
    pub to: &'a str,
    pub failures: &'a [Failure],
    /// The original message; only its header section is used.
    pub original: &'a [u8],
    pub arrival: i64,
    pub now: i64,
    /// Stable per job, so a re-run builds the same bytes.
    pub job_key: &'a str,
}

fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// The header section of `original`, CRLF-normalised, unfolded lines kept as they are, cut at `max`
/// bytes on a line boundary, with any line that could pass for a MIME boundary (`--...`) or a stray
/// body dropped. Never contains the body. Fuzzed by `fuzz_dsn_headers`.
pub fn header_section(original: &[u8], max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for raw in original.split(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            break; // the blank line ends the header section
        }
        if line.starts_with(b"--") || line.contains(&0) {
            continue;
        }
        let cleaned: Vec<u8> = line.iter().copied().filter(|&b| b == b'\t' || !b.is_ascii_control()).collect();
        if out.len() + cleaned.len() + 2 > max {
            break;
        }
        out.extend_from_slice(&cleaned);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn boundary(job_key: &str, failures: &[Failure]) -> String {
    let mut h = Sha256::new();
    h.update(job_key.as_bytes());
    for f in failures {
        h.update(f.recipient.as_bytes());
    }
    format!("rusty-dsn-{}", hex::encode(h.finalize()).get(..24).unwrap_or_default())
}

/// The id a bounce is delivered under: the same failures of the same job always give the same id, so a
/// crash after the bounce was stored and before the job was finished cannot produce a second bounce.
pub fn delivery_id(job_key: &str, failures: &[Failure]) -> DeliveryId {
    let mut recipients: Vec<&str> = failures.iter().map(|f| f.recipient.as_str()).collect();
    recipients.sort_unstable();
    DeliveryId::derive(&BlobHash::of(recipients.join("\n").as_bytes()), &format!("dsn:{job_key}"), "dsn")
}

/// Build the bounce (the whole RFC 5322 message, CRLF line ends).
pub fn build(input: &DsnInput<'_>) -> Vec<u8> {
    let host = clean(input.hostname, 253);
    let b = boundary(input.job_key, input.failures);
    let mut m = String::new();
    m.push_str("Return-Path: <>\r\n");
    m.push_str(&format!("From: Mail Delivery System <MAILER-DAEMON@{host}>\r\n"));
    m.push_str(&format!("To: <{}>\r\n", clean(input.to, 320)));
    m.push_str("Subject: Undelivered Mail Returned to Sender\r\n");
    m.push_str(&format!("Date: {}\r\n", rfc5322_date(input.now)));
    m.push_str(&format!("Message-ID: <{b}@{host}>\r\n"));
    m.push_str("Auto-Submitted: auto-replied\r\n");
    m.push_str("MIME-Version: 1.0\r\n");
    m.push_str(&format!("Content-Type: multipart/report; report-type=delivery-status; boundary=\"{b}\"\r\n\r\n"));

    // 1. for people
    m.push_str(&format!("--{b}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n"));
    m.push_str(&format!("This is the mail system at host {host}.\r\n\r\n"));
    m.push_str("Your message could not be delivered to one or more recipients. It was not delivered:\r\n\r\n");
    for f in input.failures {
        m.push_str(&format!("  <{}>: {}\r\n", clean(&f.recipient, 320), clean(&f.diagnostic, MAX_DIAG)));
    }
    m.push_str("\r\nThe headers of your message are attached; its content is not returned.\r\n\r\n");

    // 2. for machines
    m.push_str(&format!("--{b}\r\nContent-Type: message/delivery-status\r\n\r\n"));
    m.push_str(&format!("Reporting-MTA: dns; {host}\r\nArrival-Date: {}\r\n", rfc5322_date(input.arrival)));
    for f in input.failures {
        m.push_str(&format!("\r\nFinal-Recipient: rfc822; {}\r\nAction: failed\r\nStatus: {}\r\n", clean(&f.recipient, 320), clean(&f.status, 16)));
        if let Some(r) = &f.remote_mta {
            m.push_str(&format!("Remote-MTA: dns; {}\r\n", clean(r, 253)));
        }
        m.push_str(&format!("Diagnostic-Code: smtp; {}\r\nLast-Attempt-Date: {}\r\n", clean(&f.diagnostic, MAX_DIAG), rfc5322_date(input.now)));
    }
    m.push_str("\r\n");

    // 3. the original headers, not the body
    m.push_str(&format!("--{b}\r\nContent-Type: text/rfc822-headers\r\n\r\n"));
    let mut bytes = m.into_bytes();
    bytes.extend_from_slice(&header_section(input.original, MAX_HEADERS_BYTES));
    bytes.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    bytes
}
