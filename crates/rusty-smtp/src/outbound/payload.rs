//! The payload of an `outbound.send` job, decoded and validated. The job queue stores JSON; this is
//! the ONE place that turns it into typed data, and everything in it ends up in SMTP commands, so it is
//! validated as hostile input (a corrupt or tampered row must not become a command injection).
//! Fuzzed by `fuzz_queue_payload`.
//!
//! `tries` counts completed delivery attempts and is the only state the worker adds; it is how the
//! retry schedule advances across `JobQueue::reschedule` (which resets the queue's own attempt
//! counter). Recipients leave the list as they are delivered or bounced, so a retry after a partial
//! success only addresses the ones still pending.

use rusty_core::{BlobHash, Error, Result};
use serde_json::{json, Value};

/// Most recipients in one job (the submission cap is lower; this bounds a hostile payload).
pub const MAX_RECIPIENTS: usize = 1000;
/// Longest address (RFC 5321 path limit).
pub const MAX_ADDRESS: usize = 320;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPayload {
    pub blob: BlobHash,
    /// Empty for the null sender (no DSN is ever generated for it).
    pub envelope_from: String,
    pub recipients: Vec<String>,
    /// Destination domain, lower-case, as the queue split the message.
    pub destination: String,
    /// The sender's tenant domain.
    pub sender_domain: String,
    /// Completed delivery attempts.
    pub tries: u32,
}

/// An address that may go into `MAIL FROM:<..>` / `RCPT TO:<..>`: one `@`, a non-empty local part and
/// domain, no controls, no whitespace, no angle brackets or quotes (a quoted local part is not
/// something we accept from a submission, so none can reach here legitimately), bounded length.
pub fn valid_address(a: &str) -> bool {
    if a.is_empty() || a.len() > MAX_ADDRESS {
        return false;
    }
    let Some((local, domain)) = a.rsplit_once('@') else { return false };
    let bad = |c: char| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\\' | ',' | ';' | ':' | '[' | ']' | '(' | ')');
    !local.is_empty() && !domain.is_empty() && !local.chars().any(bad) && !domain.chars().any(bad) && !local.contains('@')
}

fn valid_domain(d: &str) -> bool {
    !d.is_empty() && d.len() <= 253 && !d.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '@' | '"'))
}

impl OutboundPayload {
    pub fn decode(v: &Value) -> Result<OutboundPayload> {
        let bad = |m: &str| Error::Invalid(format!("outbound job payload: {m}"));
        let obj = v.as_object().ok_or_else(|| bad("not an object"))?;
        let s = |k: &str| obj.get(k).and_then(Value::as_str).ok_or_else(|| bad(k));
        let blob = BlobHash::parse(s("blob")?)?;
        let envelope_from = s("envelope_from")?.to_owned();
        if !envelope_from.is_empty() && !valid_address(&envelope_from) {
            return Err(bad("envelope_from"));
        }
        let destination = s("destination")?.to_ascii_lowercase();
        let sender_domain = s("sender_domain")?.to_ascii_lowercase();
        if !valid_domain(&destination) || !valid_domain(&sender_domain) {
            return Err(bad("domain"));
        }
        let list = obj.get("recipients").and_then(Value::as_array).ok_or_else(|| bad("recipients"))?;
        if list.is_empty() || list.len() > MAX_RECIPIENTS {
            return Err(bad("recipient count"));
        }
        let mut recipients = Vec::with_capacity(list.len());
        for r in list {
            let r = r.as_str().ok_or_else(|| bad("recipient"))?;
            if !valid_address(r) {
                return Err(bad("recipient address"));
            }
            let domain = r.rsplit_once('@').map_or("", |(_, d)| d).to_ascii_lowercase();
            if domain != destination {
                return Err(bad("recipient outside the destination domain"));
            }
            recipients.push(r.to_owned());
        }
        let tries = match obj.get("tries") {
            None => 0,
            Some(t) => u32::try_from(t.as_u64().ok_or_else(|| bad("tries"))?).map_err(|_| bad("tries"))?.min(100_000),
        };
        Ok(OutboundPayload { blob, envelope_from, recipients, destination, sender_domain, tries })
    }

    pub fn encode(&self) -> Value {
        json!({
            "blob": self.blob.as_str(),
            "envelope_from": self.envelope_from,
            "recipients": self.recipients,
            "destination": self.destination,
            "sender_domain": self.sender_domain,
            "tries": self.tries,
        })
    }
}
