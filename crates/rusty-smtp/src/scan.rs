//! The virus and spam scanners as inbound filters (docs/design-filters-and-resolver.md): [`ClamdFilter`] speaks clamd's `INSTREAM` over TCP, [`RspamdFilter`] speaks rspamd's `POST /checkv2`.
//! Both run in the SMTP session after DATA and before `250`, so a refusal is a real SMTP refusal (no bounce, no backscatter).
//!
//! * A scanner's answer is **bounded input**: a reply is read up to a small limit and anything else is an error. The connection has a deadline; a scanner that is down, slow or speaks
//!   nonsense is handled by its `on_error` (`accept`: the message goes on, marked `unavailable`; `tempfail`: `451 4.7.1`, nothing stored).
//! * A message above `max_scan_bytes` is not sent to the scanner and goes on with `X-Scan-Status: skipped-size`.
//! * What is put in the message is a header and nothing else: `X-Spam-Status`, `X-Spam-Score`, `X-Virus-Scan`, `X-Scan-Status` (the `X-Rusty-` names stay reserved for the server itself).
//!   What is logged is a score and an action, never the message.
//! * A virus is refused with `550 5.7.1` and **no signature name** (what the sender learns is that the message was refused, not how it was recognised).

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::config::OnError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::filter::{FilterContext, FilterVerdict, InboundFilter, QuarantineFolder};

/// Told what every scan came to: (scanner, result), both from fixed sets (`clean`, `spam`, `virus`, `deferred`, `skipped`, `unknown`, `error`). The daemon counts them for the metrics page.
pub type Observer = std::sync::Arc<dyn Fn(&'static str, &'static str) + Send + Sync>;

/// The spam score lines of the recipients of a message (docs/design-sender-policy.md): at or above `junk` the message goes to the Spam folder, at or above `reject` it is refused. They are
/// the keys `spam_junk_score` and `spam_reject_score` of the configuration layers, so a plan (or a domain, or an account) has its own.
#[async_trait]
pub trait ThresholdSource: Send + Sync {
    /// `(junk, reject)` for the message's recipients; `(None, None)` leaves the decision to rspamd's own actions.
    async fn thresholds(&self, accounts: &[rusty_core::AccountId]) -> (Option<f64>, Option<f64>);
}

/// The thresholds the directory's layers give. The first recipient's account decides: after DATA one message has one fate for all of its recipients.
pub struct DirectoryThresholds(pub rusty_directory::Directory);

#[async_trait]
impl ThresholdSource for DirectoryThresholds {
    async fn thresholds(&self, accounts: &[rusty_core::AccountId]) -> (Option<f64>, Option<f64>) {
        let Some(first) = accounts.first() else { return (None, None) };
        match self.0.effective_configs(*first, &["spam_junk_score", "spam_reject_score"]).await {
            Ok(v) => (v.first().and_then(|x| x.as_ref()).and_then(serde_json::Value::as_f64), v.get(1).and_then(|x| x.as_ref()).and_then(serde_json::Value::as_f64)),
            Err(_) => (None, None),
        }
    }
}

/// What the score says under thresholds: `Some(true)` refuse, `Some(false)` junk, `None` neither line is reached.
pub fn judge(score: f64, junk: Option<f64>, reject: Option<f64>) -> Option<bool> {
    if reject.is_some_and(|r| score >= r) {
        Some(true)
    } else if junk.is_some_and(|j| score >= j) {
        Some(false)
    } else {
        None
    }
}

/// The longest reply read from a scanner.
const MAX_REPLY: usize = 1024 * 1024;
/// clamd's `INSTREAM` chunk size.
const CHUNK: usize = 64 * 1024;

fn tag(name: &str, value: &str) -> FilterVerdict {
    FilterVerdict::Tag(vec![(name.to_owned(), value.to_owned())])
}

fn error_verdict(scanner: &'static str, on_error: OnError, header: &str) -> FilterVerdict {
    tracing::warn!(scanner, "a scanner could not scan the message");
    match on_error {
        OnError::Accept => tag(header, "unavailable"),
        OnError::Tempfail => FilterVerdict::Reject { code: 451, enhanced: "4.7.1", text: "Temporary scanning problem, please try again later".to_owned() },
    }
}

async fn connect(addr: &str, timeout: Duration) -> std::io::Result<TcpStream> {
    let s = tokio::time::timeout(timeout, TcpStream::connect(addr)).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
    s.set_nodelay(true)?;
    Ok(s)
}

// ------------------------------------------------------------------------------------------------ clamd

/// clamd over TCP: `zINSTREAM\0`, the message in length-prefixed chunks, a zero length; the reply is `stream: OK`, `stream: <name> FOUND` or `... ERROR`, NUL terminated.
pub struct ClamdFilter {
    addr: String,
    timeout: Duration,
    max_bytes: u64,
    on_error: OnError,
    observer: Option<Observer>,
}

impl ClamdFilter {
    pub fn new(addr: &str, timeout: Duration, max_bytes: u64, on_error: OnError) -> ClamdFilter {
        ClamdFilter { addr: addr.to_owned(), timeout, max_bytes, on_error, observer: None }
    }

    pub fn with_observer(mut self, observer: Observer) -> ClamdFilter {
        self.observer = Some(observer);
        self
    }

    fn note(&self, result: &'static str) {
        if let Some(o) = &self.observer {
            o("clamd", result);
        }
    }

    async fn scan(&self, data: &[u8]) -> std::io::Result<Reply> {
        let mut s = connect(&self.addr, self.timeout).await?;
        let work = async {
            s.write_all(b"zINSTREAM\0").await?;
            for chunk in data.chunks(CHUNK) {
                s.write_all(&u32::try_from(chunk.len()).unwrap_or(u32::MAX).to_be_bytes()).await?;
                s.write_all(chunk).await?;
            }
            s.write_all(&0u32.to_be_bytes()).await?;
            s.flush().await?;
            let mut reply = Vec::new();
            // The reply is short; a server that sends more than a few KiB is not clamd.
            (&mut s).take(4096).read_to_end(&mut reply).await?;
            Ok::<_, std::io::Error>(reply)
        };
        let reply = tokio::time::timeout(self.timeout, work).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
        Ok(parse_clamd(&reply))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    Clean,
    Found,
    Error,
}

pub(crate) fn parse_clamd(reply: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(reply);
    let text = text.trim_end_matches(['\0', '\n', '\r', ' ']);
    if text == "stream: OK" {
        Reply::Clean
    } else if text.starts_with("stream: ") && text.ends_with(" FOUND") {
        Reply::Found
    } else {
        Reply::Error
    }
}

#[async_trait]
impl InboundFilter for ClamdFilter {
    async fn check(&self, ctx: &FilterContext<'_>) -> FilterVerdict {
        if ctx.data.len() as u64 > self.max_bytes {
            self.note("skipped");
            return tag("X-Scan-Status", "skipped-size");
        }
        match self.scan(ctx.data).await {
            Ok(Reply::Clean) => {
                self.note("clean");
                tag("X-Virus-Scan", "clean")
            }
            Ok(Reply::Found) => {
                self.note("virus");
                tracing::info!(scanner = "clamd", "a message was refused: virus");
                FilterVerdict::Reject { code: 550, enhanced: "5.7.1", text: "Message refused: it contains a virus".to_owned() }
            }
            Ok(Reply::Error) | Err(_) => {
                self.note("error");
                error_verdict("clamd", self.on_error, "X-Virus-Scan")
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------ rspamd

/// rspamd's HTTP API: `POST /checkv2` with the message as the body and the envelope in headers; the reply is JSON with `action`, `score` and `required_score`.
pub struct RspamdFilter {
    /// `host:port`.
    addr: String,
    timeout: Duration,
    max_bytes: u64,
    on_error: OnError,
    observer: Option<Observer>,
    thresholds: Option<std::sync::Arc<dyn ThresholdSource>>,
}

/// An rspamd verdict, reduced to what the SMTP session needs.
#[derive(Debug, PartialEq)]
pub(crate) struct Scored {
    pub action: String,
    pub score: f64,
    pub required: f64,
}

pub(crate) fn parse_rspamd(body: &[u8]) -> Option<Scored> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let action = v.get("action")?.as_str()?.to_owned();
    let score = v.get("score")?.as_f64()?;
    let required = v.get("required_score").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    (score.is_finite() && required.is_finite()).then_some(Scored { action, score, required })
}

/// Header values must hold nothing a header cannot: printable ASCII only, no line breaks.
fn clean(v: &str) -> String {
    v.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(200).collect()
}

impl RspamdFilter {
    pub fn new(url: &str, timeout: Duration, max_bytes: u64, on_error: OnError) -> RspamdFilter {
        let addr = url.trim_start_matches("http://").trim_end_matches('/').to_owned();
        RspamdFilter { addr, timeout, max_bytes, on_error, observer: None, thresholds: None }
    }

    /// Judge the score by the recipients' own thresholds instead of rspamd's actions (when they have any).
    pub fn with_thresholds(mut self, source: std::sync::Arc<dyn ThresholdSource>) -> RspamdFilter {
        self.thresholds = Some(source);
        self
    }

    pub fn with_observer(mut self, observer: Observer) -> RspamdFilter {
        self.observer = Some(observer);
        self
    }

    fn note(&self, result: &'static str) {
        if let Some(o) = &self.observer {
            o("rspamd", result);
        }
    }

    async fn scan(&self, ctx: &FilterContext<'_>) -> std::io::Result<Scored> {
        let mut s = connect(&self.addr, self.timeout).await?;
        let rcpts: String = ctx.recipients.iter().map(|r| format!("Rcpt: {}\r\n", clean(&r.address))).collect();
        let head = format!(
            "POST /checkv2 HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nContent-Type: message/rfc822\r\nConnection: close\r\nIP: {}\r\nHelo: {}\r\nFrom: {}\r\n{}{}",
            self.addr,
            ctx.data.len(),
            ctx.peer,
            clean(ctx.helo),
            clean(ctx.mail_from),
            rcpts,
            if ctx.tls { "TLS: yes\r\n" } else { "" },
        );
        let work = async {
            s.write_all(head.as_bytes()).await?;
            s.write_all(b"\r\n").await?;
            s.write_all(ctx.data).await?;
            s.flush().await?;
            let mut reply = Vec::new();
            (&mut s).take(MAX_REPLY as u64 + 1).read_to_end(&mut reply).await?;
            Ok::<_, std::io::Error>(reply)
        };
        let reply = tokio::time::timeout(self.timeout, work).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
        if reply.len() > MAX_REPLY {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let bad = || std::io::Error::from(std::io::ErrorKind::InvalidData);
        let split = reply.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(bad)?;
        let (status, body) = (reply.get(..split).ok_or_else(bad)?, reply.get(split + 4..).ok_or_else(bad)?);
        let first = status.split(|b| *b == b'\r').next().unwrap_or_default();
        if !first.starts_with(b"HTTP/1.1 200") && !first.starts_with(b"HTTP/1.0 200") {
            return Err(bad());
        }
        // `Connection: close`: the body is what follows (a chunked body is not asked for and is refused as nonsense).
        if status.windows(8).any(|w| w.eq_ignore_ascii_case(b"chunked\r")) || status.ends_with(b"chunked") {
            return Err(bad());
        }
        parse_rspamd(body).ok_or_else(bad)
    }
}

#[async_trait]
impl InboundFilter for RspamdFilter {
    async fn check(&self, ctx: &FilterContext<'_>) -> FilterVerdict {
        if ctx.data.len() as u64 > self.max_bytes {
            self.note("skipped");
            return tag("X-Scan-Status", "skipped-size");
        }
        let r = match self.scan(ctx).await {
            Ok(r) => r,
            Err(_) => {
                self.note("error");
                return error_verdict("rspamd", self.on_error, "X-Spam-Status");
            }
        };
        self.note(match r.action.as_str() {
            "no action" => "clean",
            "reject" | "add header" | "rewrite subject" => "spam",
            "soft reject" | "greylist" => "deferred",
            _ => "unknown",
        });
        tracing::info!(scanner = "rspamd", action = %clean(&r.action), score = r.score, "a message was scored");
        let status = |verdict: &str| {
            FilterVerdict::Tag(vec![
                ("X-Spam-Status".to_owned(), format!("{verdict}, score={:.2} required={:.2}", r.score, r.required)),
                ("X-Spam-Score".to_owned(), format!("{:.2}", r.score)),
            ])
        };
        // The recipients' own lines (their plan, domain or account): when they have any, the score is judged by them; rspamd's reject, add header and no action are then only the score.
        if let Some(source) = &self.thresholds {
            let accounts: Vec<rusty_core::AccountId> = ctx.recipients.iter().flat_map(|x| x.accounts.iter().copied()).collect();
            let (junk, reject) = source.thresholds(&accounts).await;
            if junk.is_some() || reject.is_some() {
                if matches!(r.action.as_str(), "soft reject" | "greylist") {
                    return FilterVerdict::Reject { code: 451, enhanced: "4.7.1", text: "Temporarily refused, please try again later".to_owned() };
                }
                return match judge(r.score, junk, reject) {
                    Some(true) => FilterVerdict::Reject { code: 550, enhanced: "5.7.1", text: "Message refused as spam".to_owned() },
                    Some(false) => {
                        let FilterVerdict::Tag(tags) = status("Yes") else { return FilterVerdict::Quarantine(QuarantineFolder::Spam) };
                        FilterVerdict::QuarantineTagged(QuarantineFolder::Spam, tags)
                    }
                    None => status("No"),
                };
            }
        }
        match r.action.as_str() {
            "reject" => FilterVerdict::Reject { code: 550, enhanced: "5.7.1", text: "Message refused as spam".to_owned() },
            // The sender is asked to come back (greylisting, or a score near the line): a real server retries, a spam run usually does not.
            "soft reject" | "greylist" => FilterVerdict::Reject { code: 451, enhanced: "4.7.1", text: "Temporarily refused, please try again later".to_owned() },
            // Delivered, filed in the spam folder and marked.
            "add header" | "rewrite subject" => {
                let FilterVerdict::Tag(tags) = status("Yes") else { return FilterVerdict::Quarantine(QuarantineFolder::Spam) };
                FilterVerdict::QuarantineTagged(QuarantineFolder::Spam, tags)
            }
            "no action" => status("No"),
            // An action this version does not know is not a reason to lose mail: delivered and marked.
            _ => status("Unknown"),
        }
    }
}
