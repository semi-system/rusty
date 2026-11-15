//! `[filter]`: the spam and virus scanners the inbound SMTP service asks before it answers `250` (docs/design-filters-and-resolver.md).
//!
//! Absent: no scanning. Each scanner is optional and has its own `on_error` (what a scanner that is down or slow means for the message): rspamd defaults to `accept` (mail must not stop
//! because the spam scorer is down), clamd to `tempfail` (a message that could not be scanned for viruses is not delivered: the sender is told to retry with a 451).
//!
//! The scanners are on a private network of the deployment that has no route to the internet and no access to the database; the connection to them is plain TCP/HTTP inside it. This
//! module checks the shape only (the addresses are `host:port`, resolved at connect time).

use serde::Deserialize;

use crate::{Error, Result};

/// What a scanner that cannot be reached, is too slow or answers nonsense means for the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum OnError {
    /// Deliver the message, marked as not scanned.
    #[serde(rename = "accept")]
    Accept,
    /// Answer `451`: the sender retries later and nothing is stored.
    #[serde(rename = "tempfail")]
    Tempfail,
}

/// `[filter]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    /// `http://host:port` of rspamd's normal worker (`POST /checkv2`). Absent: no spam scoring.
    pub rspamd_url: Option<String>,
    /// `host:port` of clamd's TCP socket (`INSTREAM`). Absent: no virus scan.
    pub clamd_addr: Option<String>,
    #[serde(default = "d_rspamd_on_error")]
    pub rspamd_on_error: OnError,
    #[serde(default = "d_clamd_on_error")]
    pub clamd_on_error: OnError,
    #[serde(default = "d_rspamd_timeout")]
    pub rspamd_timeout_secs: u64,
    #[serde(default = "d_clamd_timeout")]
    pub clamd_timeout_secs: u64,
    /// A message larger than this is not sent to a scanner (rspamd's `max_message` and clamd's `StreamMaxLength` are the servers' own limits): it is delivered with the header
    /// `X-Scan-Status: skipped-size`.
    #[serde(default = "d_max_scan")]
    pub max_scan_bytes: u64,
}

fn d_rspamd_on_error() -> OnError {
    OnError::Accept
}
fn d_clamd_on_error() -> OnError {
    OnError::Tempfail
}
fn d_rspamd_timeout() -> u64 {
    10
}
fn d_clamd_timeout() -> u64 {
    30
}
fn d_max_scan() -> u64 {
    25 * 1024 * 1024
}

fn host_port_ok(v: &str) -> bool {
    match v.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty()
                && host.len() <= 253
                && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
                && port.parse::<u16>().is_ok_and(|p| p != 0)
        }
        None => false,
    }
}

impl FilterConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(u) = &self.rspamd_url {
            let ok = u.strip_prefix("http://").is_some_and(|rest| host_port_ok(rest.strip_suffix('/').unwrap_or(rest)));
            if !ok {
                return Err(Error::Invalid("filter.rspamd_url must be http://host:port".into()));
            }
        }
        if let Some(a) = &self.clamd_addr {
            if !host_port_ok(a) {
                return Err(Error::Invalid("filter.clamd_addr must be host:port".into()));
            }
        }
        if self.rspamd_url.is_none() && self.clamd_addr.is_none() {
            return Err(Error::Invalid("[filter] names no scanner: set rspamd_url and/or clamd_addr, or remove the section".into()));
        }
        for (v, what) in [(self.rspamd_timeout_secs, "rspamd_timeout_secs"), (self.clamd_timeout_secs, "clamd_timeout_secs")] {
            if !(1..=300).contains(&v) {
                return Err(Error::Invalid(format!("filter.{what} must be 1 to 300")));
            }
        }
        if !(1024..=1024 * 1024 * 1024).contains(&self.max_scan_bytes) {
            return Err(Error::Invalid("filter.max_scan_bytes must be 1 KiB to 1 GiB".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &str) -> Result<FilterConfig> {
        let c: FilterConfig = toml::from_str(extra).map_err(|_| Error::Invalid("syntax".into()))?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn the_defaults_let_mail_through_without_the_spam_scorer_and_hold_it_without_the_virus_scanner() {
        let c = parse("rspamd_url = \"http://rspamd:11333\"\nclamd_addr = \"clamd:3310\"\n").unwrap();
        assert_eq!((c.rspamd_on_error, c.clamd_on_error), (OnError::Accept, OnError::Tempfail));
        assert_eq!((c.rspamd_timeout_secs, c.clamd_timeout_secs, c.max_scan_bytes), (10, 30, 25 * 1024 * 1024));
    }

    #[test]
    fn what_is_not_an_address_a_scanner_or_a_sane_limit_is_refused() {
        for bad in [
            "",
            "rspamd_url = \"https://rspamd:11333\"\n",
            "rspamd_url = \"http://rspamd\"\n",
            "rspamd_url = \"http://rspamd:0\"\n",
            "rspamd_url = \"http://rs pamd:1\"\n",
            "clamd_addr = \"clamd\"\n",
            "clamd_addr = \"clamd:99999\"\n",
            "clamd_addr = \"clamd:3310\"\nclamd_timeout_secs = 0\n",
            "clamd_addr = \"clamd:3310\"\nmax_scan_bytes = 10\n",
            "clamd_addr = \"clamd:3310\"\nunknown = 1\n",
            "clamd_addr = \"clamd:3310\"\nclamd_on_error = \"ignore\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(parse("clamd_addr = \"clamd:3310\"\nclamd_on_error = \"accept\"\n").is_ok());
        assert!(parse("rspamd_url = \"http://10.0.0.7:11333/\"\n").is_ok());
    }
}
