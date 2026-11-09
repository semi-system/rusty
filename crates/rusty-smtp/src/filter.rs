//! `InboundFilter`: the seam for spam, virus and authentication checks (rspamd, ClamAV, SPF/DKIM).
//! The pipeline runs after DATA and before delivery. Filters see the message and the envelope and
//! answer with a verdict; the default pipeline is empty (the log-only SPF/DKIM checker arrives with
//! the outbound work). A filter may refuse the message (so the sender gets the rejection: no
//! backscatter), put it in the spam folder, or add headers.

use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;

use crate::reply::Reply;
use crate::session::Recipient;

/// Everything a filter may look at. `data` is the original content as received.
pub struct FilterContext<'a> {
    pub peer: IpAddr,
    pub tls: bool,
    pub helo: &'a str,
    /// Empty for the null sender.
    pub mail_from: &'a str,
    pub recipients: &'a [Recipient],
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineFolder {
    Spam,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterVerdict {
    Accept,
    /// Refuse at the end of DATA with this status (`code` 400..=599).
    Reject { code: u16, enhanced: &'static str, text: String },
    /// Deliver into a system folder instead of the Inbox.
    Quarantine(QuarantineFolder),
    /// Add these headers (`X-` names only).
    Tag(Vec<(String, String)>),
}

#[async_trait]
pub trait InboundFilter: Send + Sync {
    async fn check(&self, ctx: &FilterContext<'_>) -> FilterVerdict;
}

/// What the whole pipeline decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FilterOutcome {
    pub reject: Option<Reply>,
    pub quarantine: Option<QuarantineFolder>,
    pub tags: Vec<(String, String)>,
}

const MAX_TAGS: usize = 20;
const MAX_TAG_VALUE: usize = 998;

fn valid_tag(name: &str, value: &str) -> bool {
    let name_ok = name.len() <= 64
        && name.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("x-"))
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("x-rusty-"));
    name_ok && value.len() <= MAX_TAG_VALUE && !value.chars().any(char::is_control)
}

/// Run the filters in order. The first rejection stops the pipeline; a quarantine verdict sticks.
pub async fn run_filters(filters: &[Arc<dyn InboundFilter>], ctx: &FilterContext<'_>) -> FilterOutcome {
    let mut outcome = FilterOutcome::default();
    for f in filters {
        match f.check(ctx).await {
            FilterVerdict::Accept => {}
            FilterVerdict::Reject { code, enhanced, text } => {
                let code = if (400..600).contains(&code) { code } else { 451 };
                outcome.reject = Some(Reply::new(code, enhanced, text));
                return outcome;
            }
            FilterVerdict::Quarantine(q) => outcome.quarantine = Some(q),
            FilterVerdict::Tag(tags) => {
                for (name, value) in tags {
                    if outcome.tags.len() < MAX_TAGS && valid_tag(&name, &value) {
                        outcome.tags.push((name, value));
                    } else {
                        tracing::warn!("an inbound filter returned an unusable header tag; dropped");
                    }
                }
            }
        }
    }
    outcome
}
