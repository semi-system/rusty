//! The I/O driver of the inbound (port 25) service: owns the stream, the timeouts and STARTTLS,
//! feeds the sans-I/O [`Session`], and answers its questions through the `Directory` and `Mailbox`
//! facades of `rusty_svc::Context`. It never names a backend.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{AccountId, BlobHash, Error, FolderId, Flags};
use rusty_directory::{parse_address, AccountStatus, Directory, DomainStatus, Resolved};
use rusty_store::repo::SYSTEM_FOLDER_TABLE;
use rusty_svc::{BoxedStream, Context, MailboxHub, PeerInfo, QuotaPolicy, SessionHandler, TlsProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::filter::{run_filters, FilterContext, InboundFilter, QuarantineFolder};
use crate::message::{delivery_id_for, prepare_message_with, Trace};
use crate::reply::{delivery_reply, map_error, ErrorContext, Reply};
use crate::session::{Action, Answer, PeerFacts, Phase, RcptVerdict, Recipient, Session, SessionConfig, Transaction};

/// Most accounts one recipient (a list) may expand to; more is `452 4.5.3`.
const MAX_ACCOUNTS_PER_RECIPIENT: usize = 1000;
/// Read buffer of the driver.
const READ_BUF: usize = 16 * 1024;

/// Wall-clock seconds (the `Received` header). Injected so tests need no real time.
pub trait WallClock: Send + Sync {
    fn now_unix(&self) -> i64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now_unix(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

pub struct InboundOptions {
    /// The inbound filter pipeline (empty by default).
    pub filters: Vec<Arc<dyn InboundFilter>>,
    pub clock: Arc<dyn WallClock>,
    /// Idle limit while waiting for a command.
    pub command_idle: Duration,
    /// Idle limit while waiting for message data.
    pub data_idle: Duration,
    /// How long the TLS handshake after STARTTLS may take.
    pub starttls_timeout: Duration,
    pub max_transactions: u32,
    pub max_commands_without_transaction: u32,
    pub max_errors: u32,
    pub max_line: usize,
}

impl Default for InboundOptions {
    fn default() -> InboundOptions {
        InboundOptions {
            filters: Vec::new(),
            clock: Arc::new(SystemWallClock),
            command_idle: Duration::from_secs(300),
            data_idle: Duration::from_secs(600),
            starttls_timeout: Duration::from_secs(10),
            max_transactions: 5,
            max_commands_without_transaction: 100,
            max_errors: 20,
            max_line: 1000,
        }
    }
}

pub struct InboundHandler {
    directory: Directory,
    hub: MailboxHub,
    quota: Arc<dyn QuotaPolicy>,
    tls: TlsProvider,
    hostname: String,
    session: SessionConfig,
    options: InboundOptions,
}

fn system_folder(role: &str) -> FolderId {
    let id = SYSTEM_FOLDER_TABLE.iter().find(|(_, _, r)| *r == role).map_or(1, |(id, _, _)| *id);
    FolderId(id)
}

fn hex12(hash: &BlobHash) -> String {
    hash.as_str().chars().take(12).collect()
}

enum Flow {
    Go,
    Stop,
}

impl InboundHandler {
    /// Built from the shared `Context` (facades and seams) and the service options.
    pub fn new(ctx: &Context, options: InboundOptions) -> InboundHandler {
        let hostname = ctx.cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
        let session = SessionConfig {
            hostname: hostname.clone(),
            max_message_bytes: ctx.cfg.smtp_max_message_bytes(),
            max_recipients: usize::try_from(ctx.cfg.smtp.max_recipients).unwrap_or(usize::MAX),
            max_transactions: options.max_transactions,
            max_commands_without_transaction: options.max_commands_without_transaction,
            max_errors: options.max_errors,
            max_line: options.max_line,
            starttls_available: ctx.tls.is_available(),
        };
        InboundHandler {
            directory: ctx.directory.clone(),
            hub: ctx.hub.clone(),
            quota: ctx.quota.clone(),
            tls: ctx.tls.clone(),
            hostname,
            session,
            options,
        }
    }

    /// Is `address` a recipient this server accepts mail for, and which accounts get it?
    async fn resolve_rcpt(&self, address: &str) -> RcptVerdict {
        let reject = |code, enh, text: &str| RcptVerdict::Reject(Reply::new(code, enh, text));
        let Ok((local, domain)) = parse_address(address) else {
            return RcptVerdict::Reject(map_error(&Error::Invalid(String::new()), ErrorContext::Rcpt));
        };
        let normal = format!("{local}@{domain}");
        let ids: Vec<AccountId> = match self.directory.resolve(&normal).await {
            Ok(Resolved::Account(id)) => vec![id],
            Ok(Resolved::List(members)) => {
                // External members need the outbound queue (not built yet); only local ones receive.
                members.into_iter().filter_map(|m| if let Resolved::Account(id) = m { Some(id) } else { None }).collect()
            }
            Ok(Resolved::External(_)) => Vec::new(),
            Err(Error::NotFound(_)) => {
                return match self.directory.find_domain(&domain).await {
                    Ok(Some(d)) if d.status == DomainStatus::Active => reject(550, "5.1.1", "No such user here"),
                    Ok(_) => reject(554, "5.7.1", "Relay access denied"),
                    Err(e) => RcptVerdict::Reject(map_error(&e, ErrorContext::Rcpt)),
                };
            }
            Err(e) => return RcptVerdict::Reject(map_error(&e, ErrorContext::Rcpt)),
        };
        let mut seen = HashSet::new();
        let unique: Vec<AccountId> = ids.into_iter().filter(|a| seen.insert(*a)).collect();
        if unique.len() > MAX_ACCOUNTS_PER_RECIPIENT {
            return RcptVerdict::Reject(map_error(&Error::Quota(String::new()), ErrorContext::Rcpt));
        }
        let (mut active, mut inactive) = (Vec::new(), 0usize);
        for id in unique {
            match self.directory.account_status(id).await {
                Ok(Some(AccountStatus::Active)) => active.push(id),
                Ok(_) => inactive += 1,
                Err(e) => return RcptVerdict::Reject(map_error(&e, ErrorContext::Rcpt)),
            }
        }
        match (active.is_empty(), inactive) {
            (false, _) => RcptVerdict::Accept(Recipient { address: normal, accounts: active }),
            (true, 0) => reject(550, "5.1.1", "No such user here"),
            (true, _) => reject(550, "5.2.1", "Mailbox disabled"),
        }
    }

    async fn deliver_one(
        &self,
        account: AccountId,
        mail_from: &str,
        original: &[u8],
        stored: Bytes,
        folder: FolderId,
    ) -> Result<(), Error> {
        self.quota.check_append(account, stored.len() as u64)?;
        let mailbox = self.hub.open_for(account).await?;
        let id = delivery_id_for(mail_from, account.0, original);
        mailbox.deliver(folder, stored, Flags::default(), id).await.map(|_| ())
    }

    /// End of DATA: filters, then one idempotent delivery per distinct account.
    async fn deliver(&self, t: &Transaction) -> Reply {
        let outcome = run_filters(
            &self.options.filters,
            &FilterContext {
                peer: t.peer.ip,
                tls: t.peer.tls,
                helo: &t.helo,
                mail_from: &t.mail_from,
                recipients: &t.recipients,
                data: &t.data,
            },
        )
        .await;
        if let Some(reply) = outcome.reject {
            tracing::info!(peer = %t.peer.ip, "message refused by an inbound filter");
            return reply;
        }
        let folder = match outcome.quarantine {
            Some(QuarantineFolder::Spam) => system_folder("spam"),
            None => system_folder("inbox"),
        };
        let queue_id = hex12(&BlobHash::of(&t.data));
        let trace = Trace {
            hostname: &self.hostname,
            helo: &t.helo,
            peer: t.peer.ip,
            tls: t.peer.tls,
            mail_from: &t.mail_from,
            queue_id: &queue_id,
            received_unix: self.options.clock.now_unix(),
        };
        let stored = Bytes::from(prepare_message_with(&t.data, &trace, &outcome.tags));
        let mut seen = HashSet::new();
        let accounts: Vec<AccountId> =
            t.recipients.iter().flat_map(|r| r.accounts.iter().copied()).filter(|a| seen.insert(*a)).collect();
        let mut results = Vec::with_capacity(accounts.len());
        for a in &accounts {
            results.push(self.deliver_one(*a, &t.mail_from, &t.data, stored.clone(), folder).await);
        }
        let reply = delivery_reply(results);
        tracing::info!(
            peer = %t.peer.ip,
            queue_id = %queue_id,
            bytes = t.data.len(),
            accounts = accounts.len(),
            status = reply.code,
            "message delivery finished"
        );
        reply
    }

    async fn flush(stream: &mut BoxedStream, out: &mut Vec<u8>) -> bool {
        if out.is_empty() {
            return true;
        }
        let ok = stream.write_all(out).await.is_ok() && stream.flush().await.is_ok();
        out.clear();
        ok
    }

    /// Run queued actions. Replies are batched and flushed before anything that blocks.
    async fn execute(
        &self,
        session: &mut Session,
        queue: &mut VecDeque<Action>,
        out: &mut Vec<u8>,
        stream: &mut BoxedStream,
    ) -> Flow {
        while let Some(action) = queue.pop_front() {
            match action {
                Action::Reply(r) => out.extend_from_slice(&r.render()),
                Action::Close => {
                    Self::flush(stream, out).await;
                    return Flow::Stop;
                }
                Action::CheckRcpt { address } => {
                    let verdict = self.resolve_rcpt(&address).await;
                    queue.extend(session.answer(Answer::Rcpt(verdict)));
                }
                Action::Deliver(t) => {
                    let reply = self.deliver(&t).await;
                    queue.extend(session.answer(Answer::Delivery(reply)));
                }
                Action::StartTls => {
                    if !Self::flush(stream, out).await {
                        return Flow::Stop;
                    }
                    let plain = std::mem::replace(stream, Box::new(tokio::io::join(tokio::io::empty(), tokio::io::sink())));
                    match tokio::time::timeout(self.options.starttls_timeout, self.tls.acceptor().accept(plain)).await {
                        Ok(Ok(secured)) => {
                            *stream = Box::new(secured);
                            queue.extend(session.tls_started());
                        }
                        _ => {
                            tracing::debug!("STARTTLS handshake failed; connection dropped");
                            return Flow::Stop;
                        }
                    }
                }
            }
        }
        if Self::flush(stream, out).await { Flow::Go } else { Flow::Stop }
    }
}

#[async_trait]
impl SessionHandler for InboundHandler {
    async fn handle(&self, mut stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        let (mut session, first) = Session::new(self.session.clone(), PeerFacts { ip: peer.addr.ip(), tls: peer.tls });
        let mut queue: VecDeque<Action> = first.into();
        let mut out = Vec::new();
        let mut buf = vec![0u8; READ_BUF];
        loop {
            if matches!(self.execute(&mut session, &mut queue, &mut out, &mut stream).await, Flow::Stop)
                || session.phase() == Phase::Closed
            {
                break;
            }
            let idle = if session.phase() == Phase::Data { self.options.data_idle } else { self.options.command_idle };
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    queue.extend(session.shutdown());
                    self.execute(&mut session, &mut queue, &mut out, &mut stream).await;
                    break;
                }
                read = tokio::time::timeout(idle, stream.read(&mut buf)) => match read {
                    Err(_) => {
                        queue.extend(session.timed_out());
                        self.execute(&mut session, &mut queue, &mut out, &mut stream).await;
                        break;
                    }
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => queue.extend(session.feed(buf.get(..n).unwrap_or_default())),
                },
            }
        }
        let _ = stream.shutdown().await;
    }
}
