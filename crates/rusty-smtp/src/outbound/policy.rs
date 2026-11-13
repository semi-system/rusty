//! Retry policy as pure functions: how a remote reply is classified, how long to wait before the next
//! attempt, and when a message is given up on. No clock, no I/O: the worker passes the database
//! clock's `now` and the job's `created_at`.

use rusty_core::config::OutboundConfig;

use super::smtp_reply::{Class, Reply};

/// What one recipient's attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// Try again later (4xx, network trouble, TLS policy not met).
    Temporary { diagnostic: String },
    /// Never retry; bounce (5xx).
    Permanent { diagnostic: String, status: String },
}

/// Classify a reply that answered a command (RCPT, DATA end, MAIL). 2xx is success, 4xx temporary, 5xx
/// permanent. Anything else (1xx, 3xx where not expected, 6xx) is a broken server: temporary, so the
/// mail is not bounced because of someone else's bug.
pub fn classify(reply: &Reply) -> Outcome {
    match reply.class() {
        Class::Positive => Outcome::Delivered,
        Class::Permanent => Outcome::Permanent {
            diagnostic: reply.summary(),
            status: reply.enhanced.clone().unwrap_or_else(|| "5.0.0".to_owned()),
        },
        Class::Transient | Class::Intermediate | Class::Other => Outcome::Temporary { diagnostic: reply.summary() },
    }
}

/// What to do with a job after an attempt that left recipients undelivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Run again after this many seconds.
    Retry { after_secs: u32 },
    /// The message is too old; bounce what is left.
    GiveUp,
}

/// The delay before try `tries + 1` after `tries` completed attempts, never beyond the expiry: the
/// last retry lands exactly at `created_at + max_age` so the bounce is on time.
pub fn next_step(cfg: &OutboundConfig, created_at: i64, now: i64, tries: u32) -> Next {
    let deadline = created_at.saturating_add(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX));
    if now >= deadline {
        return Next::GiveUp;
    }
    let wanted = i64::from(cfg.backoff_for(tries));
    let left = deadline - now;
    let after = wanted.min(left).max(1);
    Next::Retry { after_secs: u32::try_from(after).unwrap_or(u32::MAX) }
}

/// The message has outlived `max_age_secs`.
pub fn expired(cfg: &OutboundConfig, created_at: i64, now: i64) -> bool {
    now >= created_at.saturating_add(i64::try_from(cfg.max_age_secs).unwrap_or(i64::MAX))
}
