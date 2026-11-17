//! The change feed: how one process hears about changes made by OTHER processes (migration 0017).
//!
//! With one process per service, a delivery by `smtp-in` cannot wake an IDLE session of `imap`
//! through memory, so the database announces it (`NOTIFY` at commit) and each process keeps ONE
//! dedicated connection that `LISTEN`s. The feed hands what it hears to a [`ChangeSink`].
//!
//! A notification is a hint, never the record: Postgres does not store them for a listener that is
//! away, so the feed calls [`ChangeSink::resync`] after every (re)subscription and the consumers
//! look at the change log / job queue themselves. They also keep their own timers (IDLE polls the
//! change log every 30 s, the outbound worker polls the queue every 2 s), so a lost notification
//! costs latency, never correctness.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{MailboxId, Result};
use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};
use tokio_util::sync::CancellationToken;

/// `NOTIFY` channel of the mailbox change log; payload `<mailbox id>:<newest seq>`.
pub const CHANNEL_MAILBOX: &str = "rusty_mailbox";
/// `NOTIFY` channel of the job queue; empty payload.
pub const CHANNEL_JOBS: &str = "rusty_jobs";

/// First wait after a lost connection; doubles up to [`BACKOFF_MAX`] and starts over once a
/// subscription worked.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A mailbox payload is two decimal numbers and a colon; anything longer is not ours.
const MAX_PAYLOAD: usize = 48;

/// Where the feed delivers. Every method must return immediately (the feed has one task for all of them).
pub trait ChangeSink: Send + Sync {
    /// A mailbox changed up to change-log sequence `seq` (monotone per mailbox).
    fn mailbox_changed(&self, mailbox: MailboxId, seq: i64);
    /// A job was queued.
    fn jobs_queued(&self);
    /// The feed has (re)subscribed: whatever was announced while it was away is lost, so look again
    /// at everything you follow.
    fn resync(&self);
    /// The feed lost its connection (or could not make one) and will retry.
    fn lost(&self) {}
}

/// A source of [`ChangeSink`] calls, run as a service of the process.
#[async_trait]
pub trait ChangeFeed: Send + Sync {
    /// Listen until `shutdown` is cancelled, then return `Ok`. A lost connection is retried with a
    /// growing delay and reported through [`ChangeSink::lost`]; it is never an error of this call.
    async fn run(&self, sink: Arc<dyn ChangeSink>, shutdown: CancellationToken) -> Result<()>;
}

/// The Postgres implementation, over its own pool of exactly one connection.
///
/// The connection is separate from the store's pool on purpose: a `LISTEN` session must stay open
/// for the life of the process (it would pin one of the pool's connections), and behind a
/// transaction-pooling proxy it must not be a pooled connection at all. It uses the store's connect
/// options, so the same server, role, TLS policy and credentials apply.
pub struct PgChangeFeed {
    pool: PgPool,
}

impl PgChangeFeed {
    pub(crate) fn new(options: sqlx::postgres::PgConnectOptions) -> PgChangeFeed {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .max_lifetime(None)
            .idle_timeout(None)
            .acquire_timeout(Duration::from_secs(10))
            .connect_lazy_with(options);
        PgChangeFeed { pool }
    }
}

/// `"<mailbox id>:<seq>"` with both numbers positive; anything else is ignored (a foreign or damaged
/// payload must not reach a sink).
pub(crate) fn parse_mailbox_payload(payload: &str) -> Option<(MailboxId, i64)> {
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let (mailbox, seq) = payload.split_once(':')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(mailbox) || !digits(seq) {
        return None;
    }
    let (mailbox, seq) = (mailbox.parse::<i64>().ok()?, seq.parse::<i64>().ok()?);
    (mailbox >= 1 && seq >= 1).then_some((MailboxId(mailbox), seq))
}

/// How one subscription ended.
enum Ended {
    Stopped,
    /// The connection could not be made, the subscription failed, or the connection dropped. The flag
    /// says whether the subscription had worked (then the next delay starts from the minimum).
    Lost { was_subscribed: bool },
}

impl PgChangeFeed {
    async fn subscription(&self, sink: &Arc<dyn ChangeSink>, shutdown: &CancellationToken) -> Ended {
        let Ok(mut listener) = PgListener::connect_with(&self.pool).await else {
            return Ended::Lost { was_subscribed: false };
        };
        // This type's own loop decides when and how fast to reconnect, and tells the sink.
        listener.eager_reconnect(false);
        if listener.listen_all([CHANNEL_MAILBOX, CHANNEL_JOBS]).await.is_err() {
            return Ended::Lost { was_subscribed: false };
        }
        // The LISTEN is in place: from here on nothing is missed, so everything before it is looked up.
        sink.resync();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ended::Stopped,
                heard = listener.try_recv() => match heard {
                    Ok(Some(n)) => match n.channel() {
                        CHANNEL_MAILBOX => {
                            if let Some((mailbox, seq)) = parse_mailbox_payload(n.payload()) {
                                sink.mailbox_changed(mailbox, seq);
                            }
                        }
                        CHANNEL_JOBS => sink.jobs_queued(),
                        _ => {}
                    },
                    Ok(None) | Err(_) => return Ended::Lost { was_subscribed: true },
                },
            }
        }
    }
}

#[async_trait]
impl ChangeFeed for PgChangeFeed {
    async fn run(&self, sink: Arc<dyn ChangeSink>, shutdown: CancellationToken) -> Result<()> {
        let mut delay = BACKOFF_MIN;
        let result = loop {
            if shutdown.is_cancelled() {
                break Ok(());
            }
            match self.subscription(&sink, &shutdown).await {
                Ended::Stopped => break Ok(()),
                Ended::Lost { was_subscribed } => {
                    sink.lost();
                    if was_subscribed {
                        delay = BACKOFF_MIN;
                    }
                    tokio::select! {
                        () = shutdown.cancelled() => break Ok(()),
                        () = tokio::time::sleep(delay) => {}
                    }
                    delay = (delay * 2).min(BACKOFF_MAX);
                }
            }
        };
        // The one connection goes back to the pool and the pool closes it: nothing is left listening.
        self.pool.close().await;
        result
    }
}

/// `NOTIFY` channel of the admin console's live updates (migration 0028); payload `<kind>:<domain id>`.
pub const CHANNEL_ADMIN: &str = "rusty_admin";

/// What a change announced on [`CHANNEL_ADMIN`] concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminKind {
    Domains,
    Accounts,
    Dns,
    Outbound,
    Storage,
    Admins,
}

/// One announcement: something of `kind` changed, in the domain with this id when the kind has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminChange {
    pub kind: AdminKind,
    pub domain: Option<i64>,
}

/// `"<kind>:<id or nothing>"`; anything else is ignored (a foreign or damaged payload must not reach a sink).
pub(crate) fn parse_admin_payload(payload: &str) -> Option<AdminChange> {
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let (kind, id) = payload.split_once(':')?;
    let kind = match kind {
        "domains" => AdminKind::Domains,
        "accounts" => AdminKind::Accounts,
        "dns" => AdminKind::Dns,
        "outbound" => AdminKind::Outbound,
        "storage" => AdminKind::Storage,
        "admins" => AdminKind::Admins,
        _ => return None,
    };
    let domain = if id.is_empty() {
        None
    } else if id.bytes().all(|b| b.is_ascii_digit()) {
        Some(id.parse::<i64>().ok().filter(|n| *n >= 1)?)
    } else {
        return None;
    };
    Some(AdminChange { kind, domain })
}

/// Where the admin feed delivers. Every method must return immediately (the feed has one task).
pub trait AdminSink: Send + Sync {
    fn changed(&self, change: AdminChange);
    /// The feed has (re)subscribed: whatever was announced while it was away is lost, so every page refreshes.
    fn resync(&self);
    /// The feed lost its connection (or could not make one) and will retry.
    fn lost(&self) {}
}

/// The admin console's own feed: one dedicated connection that LISTENs on [`CHANNEL_ADMIN`], with the retry and the `resync` of [`PgChangeFeed`].
pub struct PgAdminFeed {
    pool: PgPool,
}

impl PgAdminFeed {
    pub(crate) fn new(options: sqlx::postgres::PgConnectOptions) -> PgAdminFeed {
        let pool = PgPoolOptions::new().max_connections(1).max_lifetime(None).idle_timeout(None).acquire_timeout(Duration::from_secs(10)).connect_lazy_with(options);
        PgAdminFeed { pool }
    }

    async fn subscription(&self, sink: &Arc<dyn AdminSink>, shutdown: &CancellationToken) -> Ended {
        let Ok(mut listener) = PgListener::connect_with(&self.pool).await else {
            return Ended::Lost { was_subscribed: false };
        };
        listener.eager_reconnect(false);
        if listener.listen(CHANNEL_ADMIN).await.is_err() {
            return Ended::Lost { was_subscribed: false };
        }
        sink.resync();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ended::Stopped,
                heard = listener.try_recv() => match heard {
                    Ok(Some(n)) => {
                        if let Some(change) = parse_admin_payload(n.payload()) {
                            sink.changed(change);
                        }
                    }
                    Ok(None) | Err(_) => return Ended::Lost { was_subscribed: true },
                },
            }
        }
    }

    /// Listen until `shutdown` is cancelled, then return `Ok`. A lost connection is retried with a growing delay and reported through [`AdminSink::lost`].
    pub async fn run(&self, sink: Arc<dyn AdminSink>, shutdown: CancellationToken) -> Result<()> {
        let mut delay = BACKOFF_MIN;
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match self.subscription(&sink, &shutdown).await {
                Ended::Stopped => break,
                Ended::Lost { was_subscribed } => {
                    sink.lost();
                    if was_subscribed {
                        delay = BACKOFF_MIN;
                    }
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(delay) => {}
                    }
                    delay = (delay * 2).min(BACKOFF_MAX);
                }
            }
        }
        self.pool.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_admin_payload_is_a_known_kind_and_an_optional_positive_id() {
        assert_eq!(parse_admin_payload("storage:12"), Some(AdminChange { kind: AdminKind::Storage, domain: Some(12) }));
        assert_eq!(parse_admin_payload("domains:"), Some(AdminChange { kind: AdminKind::Domains, domain: None }));
        assert_eq!(parse_admin_payload("admins:"), Some(AdminChange { kind: AdminKind::Admins, domain: None }));
        // A store without a domain (the platform store) announces its kind with no id.
        assert_eq!(parse_admin_payload("storage:"), Some(AdminChange { kind: AdminKind::Storage, domain: None }));
        for bad in ["", ":", "storage", "storage:0", "storage:-1", "storage:x", "storage: 1", "storage:1:2", "other:1", "Storage:1", "dns:99999999999999999999", "dns:\u{0661}"] {
            assert_eq!(parse_admin_payload(bad), None, "{bad:?}");
        }
        assert_eq!(parse_admin_payload(&format!("dns:{}", "1".repeat(MAX_PAYLOAD))), None, "an over-long payload is not ours");
    }


    #[test]
    fn a_mailbox_payload_is_two_positive_numbers() {
        assert_eq!(parse_mailbox_payload("7:42"), Some((MailboxId(7), 42)));
        assert_eq!(parse_mailbox_payload("9223372036854775807:1"), Some((MailboxId(i64::MAX), 1)));
    }

    #[test]
    fn anything_else_is_ignored() {
        for bad in [
            "", ":", "7", "7:", ":7", "7:x", "x:7", "7:-1", "-7:1", "0:1", "1:0", "+7:1", " 7:1", "7:1 ", "7:1:2", "7::1",
            "99999999999999999999:1", "1:99999999999999999999", "\u{0661}:1", "7:42\n",
        ] {
            assert_eq!(parse_mailbox_payload(bad), None, "{bad:?}");
        }
        let long = format!("{}:1", "1".repeat(MAX_PAYLOAD));
        assert_eq!(parse_mailbox_payload(&long), None, "an over-long payload is not ours");
    }
}
