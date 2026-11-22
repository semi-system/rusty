//! The I/O driver of the SMTP services: port 25 (inbound mail) and, with [`SubmissionDeps`],
//! 587/465 (submission). It owns the stream, the timeouts and STARTTLS, feeds the sans-I/O
//! [`Session`], and answers its questions through the `Directory`, `Mailbox`, `Authenticator` and
//! `RateLimiter` seams of `rusty_svc::Context`. It never names a backend.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{AccountId, BlobHash, Error, FolderId, Flags};
use rusty_directory::{parse_address, AccountStatus, Authenticator, Directory, DomainStatus, Resolved};
use rusty_store::repo::SYSTEM_FOLDER_TABLE;
use rusty_store::{BlobStore, DeliverOutcome};
use rusty_svc::{
    BoxedStream, Context, MailboxHub, OutboundEnqueue, OutboundMessage, PeerInfo, QuotaPolicy, RateLimiter,
    ServiceEvent, ServiceEvents, SessionHandler, TlsProvider, Verdict,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::authres::{AuthInput, InboundAuthChecker};
use sha2::{Digest, Sha256};
use crate::filter::{run_filters, FilterContext, InboundFilter, QuarantineFolder};
use crate::limits::{MemoryBudget, SubmissionLimiter};
use crate::message::{
    delivery_id_for, from_addresses, generated_message_id, has_header, header_count, header_values, prepare_message_with, rfc5322_date, FromError, Trace,
};
use crate::reply::{delivery_reply, map_error, ErrorContext, Reply};
use crate::session::{
    Action, Answer, AuthVerdict, Identity, PeerFacts, Phase, RcptVerdict, Recipient, Secret, Session, SessionConfig,
    Transaction,
};

/// Most accounts one recipient (a list) may expand to; more is `452 4.5.3`.
const MAX_ACCOUNTS_PER_RECIPIENT: usize = 1000;
/// Most members outside this server one list may have for its mail to be forwarded (each is a recipient of the outbound job).
const MAX_FORWARD_MEMBERS: usize = 100;
/// More `Received` fields than this on a message for a list with outside members is a mail loop (RFC 5321 6.3 suggests a limit of this size).
const MAX_HOPS: usize = 25;
/// The header that marks a message as already forwarded by a list of this server, with the list address as its value.
const LIST_HEADER: &str = "X-Rusty-List";
/// Memory is reserved while a message is received in steps of this many bytes.
const RESERVE_STEP: u64 = 256 * 1024;
/// A message in flight exists about four times (received, normalised, prepared, handed to the store); the
/// reception reserves the first copy, the delivery reserves the other three.
const EXTRA_COPIES: u64 = 3;
/// Slowest DATA transfer tolerated once the grace period is over, in bytes per second.
const MIN_DATA_RATE: u64 = 1024;
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
    /// SPF/DKIM/DMARC checks for port 25 (log-only); `None` writes `none` in `Authentication-Results`.
    pub auth: Option<Arc<dyn InboundAuthChecker>>,
    pub clock: Arc<dyn WallClock>,
    /// Idle limit while waiting for a command.
    pub command_idle: Duration,
    /// Idle limit while waiting for message data.
    pub data_idle: Duration,
    /// How long the TLS handshake after STARTTLS may take.
    pub starttls_timeout: Duration,
    /// How long writing the replies may take before the peer is cut.
    pub write_timeout: Duration,
    /// Memory all sessions together may hold for messages in flight (share one across listeners).
    pub budget: MemoryBudget,
    pub max_transactions: u32,
    /// Most distinct mailboxes one message may reach (lists expand into their members).
    pub max_accounts: usize,
    pub max_commands_without_transaction: u32,
    pub max_errors: u32,
    pub max_line: usize,
}

impl Default for InboundOptions {
    fn default() -> InboundOptions {
        InboundOptions {
            filters: Vec::new(),
            auth: None,
            clock: Arc::new(SystemWallClock),
            command_idle: Duration::from_secs(300),
            data_idle: Duration::from_secs(600),
            starttls_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(30),
            budget: MemoryBudget::new_mib(1024),
            max_transactions: 5,
            max_accounts: 1000,
            max_commands_without_transaction: 100,
            max_errors: 20,
            max_line: 1000,
        }
    }
}

/// What submission needs beyond port 25: where mail for remote recipients goes, and the per-account
/// sending limit. Both are seams (constructor injection).
pub struct SubmissionDeps {
    pub outbound: Arc<dyn OutboundEnqueue>,
    pub limiter: Arc<dyn SubmissionLimiter>,
    /// The outbound worker runs: remote recipients may be accepted. `false` refuses them with a
    /// temporary status (mail is never accepted into a queue nobody drains).
    pub remote_enabled: bool,
    /// Messages an hour an account may send when its plan says nothing (the limiter's own budget).
    pub default_per_hour: u32,
}

pub struct InboundHandler {
    auth: Arc<dyn Authenticator>,
    rate: Arc<dyn RateLimiter>,
    events: Arc<dyn ServiceEvents>,
    blobs: Arc<dyn BlobStore>,
    submission: Option<SubmissionDeps>,
    /// Where mail for the outside members of a distribution list goes when it arrives on port 25 (submission uses its own queue); `None`: such a list is refused.
    forwarder: Option<Arc<dyn OutboundEnqueue>>,
    directory: Directory,
    hub: MailboxHub,
    quota: Arc<dyn QuotaPolicy>,
    plans: Arc<dyn rusty_store::PlanRepo>,
    /// The delivery log the trace of the admin web reads (docs/design-queue.md).
    queue: Arc<dyn rusty_store::QueueRepo>,
    tls: TlsProvider,
    hostname: String,
    session: SessionConfig,
    options: InboundOptions,
}

pub(crate) fn system_folder(role: &str) -> FolderId {
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
        Self::build(ctx, options, None)
    }

    /// Let a list with members outside this server be written to on this listener (port 25): the message is forwarded through `queue` (docs/design-routing.md).
    pub fn with_forwarder(mut self, queue: Arc<dyn OutboundEnqueue>) -> InboundHandler {
        self.forwarder = Some(queue);
        self
    }

    /// The submission service (587 with STARTTLS, 465 with implicit TLS): AUTH over TLS only.
    pub fn submission(ctx: &Context, options: InboundOptions, deps: SubmissionDeps) -> InboundHandler {
        Self::build(ctx, options, Some(deps))
    }

    fn build(ctx: &Context, options: InboundOptions, submission: Option<SubmissionDeps>) -> InboundHandler {
        let hostname = ctx.cfg.hostname.clone().unwrap_or_else(|| "localhost".to_owned());
        let session = SessionConfig {
            hostname: hostname.clone(),
            max_message_bytes: ctx.cfg.smtp_max_message_bytes(),
            max_recipients: usize::try_from(ctx.cfg.smtp.max_recipients).unwrap_or(usize::MAX),
            max_transactions: options.max_transactions,
            max_accounts: options.max_accounts,
            max_commands_without_transaction: options.max_commands_without_transaction,
            max_errors: options.max_errors,
            max_line: options.max_line,
            starttls_available: ctx.tls.is_available(),
            submission: submission.is_some(),
        };
        InboundHandler {
            auth: ctx.auth.clone(),
            rate: ctx.rate.clone(),
            events: ctx.events.clone(),
            blobs: ctx.blobs.clone(),
            submission,
            forwarder: None,
            directory: ctx.directory.clone(),
            hub: ctx.hub.clone(),
            quota: ctx.quota.clone(),
            plans: ctx.plans.clone(),
            queue: ctx.queue.clone(),
            tls: ctx.tls.clone(),
            hostname,
            session,
            options,
        }
    }

    /// Is `address` a recipient this server accepts mail for, and which accounts get it?
    /// `postmaster` without a domain: the first domain this server hosts that the host name belongs to
    /// (`mail.example.net` -> `example.net`).
    async fn bare_postmaster_domain(&self) -> Option<String> {
        let labels: Vec<&str> = self.hostname.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels.get(i..)?.join(".");
            if let Ok(Some(d)) = self.directory.find_domain(&candidate).await {
                if d.status == DomainStatus::Active {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// RFC 5321 4.5.1: `postmaster` is always deliverable. When the alias is missing or its target is not
    /// active, the oldest active account of the (active) domain receives it; a domain without any active
    /// account has nobody to give it to.
    async fn postmaster_fallback(&self, domain: &str) -> Option<AccountId> {
        let dom = self.directory.find_domain(domain).await.ok().flatten().filter(|d| d.status == DomainStatus::Active)?;
        let mut after = None;
        for _ in 0..5 {
            let page = self.directory.list_accounts(dom.id, after, 100).await.ok()?;
            if let Some(a) = page.iter().find(|a| a.status == AccountStatus::Active) {
                return Some(a.id);
            }
            after = Some(page.last()?.id);
        }
        None
    }

    async fn postmaster_target(&self, local: &str, domain: &str) -> Option<AccountId> {
        if local == "postmaster" {
            self.postmaster_fallback(domain).await
        } else {
            None
        }
    }

    async fn resolve_rcpt(&self, address: &str) -> RcptVerdict {
        let reject = |code, enh, text: &str| RcptVerdict::Reject(Reply::new(code, enh, text));
        let bare;
        let address = if !address.contains('@') && address.eq_ignore_ascii_case("postmaster") {
            match self.bare_postmaster_domain().await {
                Some(d) => {
                    bare = format!("postmaster@{d}");
                    bare.as_str()
                }
                None => return reject(550, "5.1.1", "No such user here"),
            }
        } else {
            address
        };
        let Ok((local, domain)) = parse_address(address) else {
            return RcptVerdict::Reject(map_error(&Error::Invalid(String::new()), ErrorContext::Rcpt));
        };
        let normal = format!("{local}@{domain}");
        // The outside members of the list this address names, when it is one that is forwarded.
        let mut forward: Vec<String> = Vec::new();
        let ids: Vec<AccountId> = match self.directory.resolve(&normal).await {
            Ok(Resolved::Account(id)) => vec![id],
            Ok(Resolved::List(members)) => {
                let outside: Vec<String> = members.iter().filter_map(|m| if let Resolved::External(e) = m { Some(e.clone()) } else { None }).collect();
                if !outside.is_empty() {
                    match (&self.submission, &self.forwarder) {
                        // A user sending while the outbound worker does not run: the same answer as for any remote recipient.
                        (Some(d), _) if !d.remote_enabled => {
                            return reject(451, "4.3.2", "Delivery to other domains is not enabled on this server; try again later");
                        }
                        // The mail is forwarded to the outside members through the outbound queue.
                        (Some(_), _) | (None, Some(_)) => {
                            if outside.len() > MAX_FORWARD_MEMBERS {
                                return reject(452, "4.5.3", "This distribution list is too large to forward");
                            }
                            forward = outside;
                        }
                        // No queue on this listener (the outbound service is not enabled): only the local members receive, as before.
                        (None, None) => {}
                    }
                }
                members.into_iter().filter_map(|m| if let Resolved::Account(id) = m { Some(id) } else { None }).collect()
            }
            Ok(Resolved::External(_)) => Vec::new(),
            Err(Error::NotFound(_)) => {
                return match self.directory.find_domain(&domain).await {
                    Ok(Some(d)) if d.status == DomainStatus::Active => match self.postmaster_target(&local, &domain).await {
                        Some(id) => RcptVerdict::Accept(Recipient { address: normal, accounts: vec![id] }),
                        None => reject(550, "5.1.1", "No such user here"),
                    },
                    // An authenticated user may send to the world; port 25 is never an open relay.
                    // A domain we host but have disabled is not "the world": never queue it to our own MX.
                    Ok(Some(_)) if self.submission.is_some() => reject(550, "5.1.2", "The recipient domain is not available here"),
                    Ok(None) if self.submission.as_ref().is_some_and(|d| !d.remote_enabled) => {
                        reject(451, "4.3.2", "Delivery to other domains is not enabled on this server; try again later")
                    }
                    Ok(None) if self.submission.is_some() => RcptVerdict::AcceptRemote(normal),
                    Ok(_) => reject(554, "5.7.1", "Relay access denied"),
                    Err(e) => RcptVerdict::Reject(map_error(&e, ErrorContext::Rcpt)),
                };
            }
            Err(e) => return RcptVerdict::Reject(map_error(&e, ErrorContext::Rcpt)),
        };
        let mut seen = HashSet::new();
        let unique: Vec<AccountId> = ids.into_iter().filter(|a| seen.insert(*a)).collect();
        // A list this large will never be accepted: permanent, so senders do not retry for days.
        if unique.len() > MAX_ACCOUNTS_PER_RECIPIENT {
            return RcptVerdict::Reject(map_error(&Error::Quota(String::new()), ErrorContext::Rcpt));
        }
        // One statement for all members (the effective status: an account of a disabled domain is disabled).
        let statuses = match self.directory.account_statuses(&unique).await {
            Ok(s) => s,
            Err(e) => return RcptVerdict::Reject(map_error(&e, ErrorContext::Rcpt)),
        };
        let (mut active, mut inactive) = (Vec::new(), 0usize);
        for id in unique {
            match statuses.get(&id) {
                // A locked account cannot sign in but its mail still arrives; a closed (disabled) one receives nothing.
                Some(AccountStatus::Active | AccountStatus::Locked) => active.push(id),
                _ => inactive += 1,
            }
        }
        if !forward.is_empty() {
            return RcptVerdict::AcceptWithForward(Recipient { address: normal, accounts: active }, forward);
        }
        match (active.is_empty(), inactive) {
            (false, _) => RcptVerdict::Accept(Recipient { address: normal, accounts: active }),
            (true, _) if local == "postmaster" => match self.postmaster_fallback(&domain).await {
                Some(id) => RcptVerdict::Accept(Recipient { address: normal, accounts: vec![id] }),
                None => reject(550, "5.2.1", "Mailbox disabled"),
            },
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
        let size = stored.len() as u64;
        match mailbox.deliver(folder, stored, Flags::default(), id).await {
            Ok(DeliverOutcome::Created(_)) => {
                self.warn_when_nearly_full(&mailbox, account, size).await;
                Ok(())
            }
            Ok(DeliverOutcome::Duplicate(_)) => Ok(()),
            // The id is derived from the ORIGINAL data, the stored bytes carry the arrival time. A retry that
            // crosses a second therefore stores different bytes under an id the store already knows and is
            // answered Conflict: the message is already delivered, which is what the retry wants to know.
            Err(Error::Conflict(_)) => {
                tracing::debug!("retry of an already delivered message");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// How many messages an hour this account may submit: `messages_per_hour` of its plan (or its own or its domain's configuration), else the limit of the service.
    async fn messages_per_hour(&self, account: AccountId) -> u32 {
        let configured = self.submission.as_ref().map_or(u32::MAX, |d| d.default_per_hour);
        match self.directory.effective_config(account, "messages_per_hour").await {
            Ok(Some(v)) => v.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(configured),
            _ => configured,
        }
    }

    /// Write what became of a message on port 25 in the delivery log: stored for each recipient, or refused with the reply that was sent. Best effort, and only for recipients of a domain
    /// this server hosts (the statement that writes the rows drops the others). At most 200 recipients are logged for one message.
    async fn trace_inbound(&self, t: &Transaction, event: &'static str, reply: Option<&Reply>) {
        let rows: Vec<rusty_store::InboundRow> = t
            .recipients
            .iter()
            .take(200)
            .map(|r| rusty_store::InboundRow {
                sender: t.mail_from.clone(),
                recipient: r.address.clone(),
                event,
                remote: t.peer.ip.to_string(),
                status: reply.map(|x| x.enhanced.to_owned()).filter(|s| !s.is_empty()),
                detail: reply.map(|x| x.lines.join(" ")),
            })
            .collect();
        if let Err(e) = self.queue.log_inbound(&rows).await {
            tracing::debug!(error = %e, "the delivery log could not be written");
        }
    }

    /// When this delivery took the mailbox past 90 % of its quota, a message from the server in its Inbox says so (docs/design-plans-quota.md). Once per limit and per retention of
    /// delivery ids (the warning is an idempotent delivery, so a retry or a second crossing a day later adds none). Best effort: a failure here never fails the delivery.
    async fn warn_when_nearly_full(&self, mailbox: &rusty_store::Mailbox, account: AccountId, size: u64) {
        let Some(limit) = mailbox.limits().quota_bytes else { return };
        let Ok(used) = self.plans.mailbox_used(mailbox.id().0).await else { return };
        let used = u64::try_from(used).unwrap_or(0);
        let before = used.saturating_sub(size);
        // 90 % without a floating point: used * 10 >= limit * 9.
        if used.saturating_mul(10) < limit.saturating_mul(9) || before.saturating_mul(10) >= limit.saturating_mul(9) {
            return;
        }
        let percent = used.saturating_mul(100) / limit.max(1);
        let body = format!(
            "From: Mail system <postmaster@{host}>\r\nTo: you\r\nSubject: Your mailbox is {percent}% full\r\nDate: {date}\r\nMessage-ID: <quota-{acct}-{limit}@{host}>\r\nAuto-Submitted: auto-generated\r\n\r\nYour mailbox holds {used_mb} MB of the {limit_mb} MB it may hold. When it is full, new mail is refused and the sender is told.\r\nDelete mail you no longer need (and empty the Trash), or ask your administrator for a larger mailbox.\r\n",
            host = self.hostname,
            date = rfc5322_date(self.options.clock.now_unix()),
            acct = account.0,
            used_mb = used / (1024 * 1024),
            limit_mb = limit / (1024 * 1024),
        );
        let id = delivery_id_for("quota-warning", account.0, format!("limit:{limit}").as_bytes());
        // The warning is the server's own message: it is stored even though the mailbox is nearly full, so it must not be held to the quota it reports.
        let unlimited = mailbox.clone().with_limits(rusty_store::MailboxLimits { quota_bytes: None, ..mailbox.limits() });
        if let Err(e) = unlimited.deliver(system_folder("inbox"), Bytes::from(body), Flags::default(), id).await {
            tracing::debug!(error = %e, "quota warning not stored");
        }
    }

    /// The checks of a message for a list with members outside this server (docs/design-routing.md): a message that has been through too many hops, or that this server's list already
    /// forwarded, is a loop and is refused for good; on port 25 only a member of the list may write to it (an outside sender could otherwise use the list to send mail to its members).
    async fn list_guard(&self, t: &Transaction) -> Option<Reply> {
        if t.forward_lists.is_empty() {
            return None;
        }
        if header_count(&t.data, "received") > MAX_HOPS {
            return Some(Reply::new(554, "5.4.6", "Too many hops: this message is looping"));
        }
        let marked = header_values(&t.data, LIST_HEADER);
        if t.forward_lists.iter().any(|l| marked.iter().any(|m| m.eq_ignore_ascii_case(l))) {
            return Some(Reply::new(554, "5.4.6", "This message has already been through this list"));
        }
        if t.identity.is_none() && !self.may_post(t).await {
            return Some(Reply::new(550, "5.7.1", "Only members may write to this list"));
        }
        None
    }

    /// Is the envelope sender a member of every list of this message with outside members: an outside member by address, or an account of this server by its address.
    async fn may_post(&self, t: &Transaction) -> bool {
        let from = t.mail_from.to_ascii_lowercase();
        if from.is_empty() {
            return false;
        }
        let sender = match self.directory.find_account(&from).await {
            Ok(Some(a)) => Some(a.id),
            _ => None,
        };
        for list in &t.forward_lists {
            match self.directory.resolve(list).await {
                Ok(Resolved::List(members)) => {
                    let member = members.iter().any(|m| match m {
                        Resolved::External(e) => e.eq_ignore_ascii_case(&from),
                        Resolved::Account(id) => Some(*id) == sender,
                        Resolved::List(_) => false,
                    });
                    if !member {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    /// The marker headers a message for these lists carries from here on.
    fn list_tags(lists: &[String]) -> Vec<(String, String)> {
        lists.iter().map(|l| (LIST_HEADER.to_owned(), l.clone())).collect()
    }

    /// Port 25: the outside members of the lists of a message go to the outbound queue, from the bounce address of the first list (a sender that is the list's own, so that what comes back
    /// is not mistaken for the original sender's mail), counted to the list's domain.
    async fn forward(&self, t: &Transaction, queue: &Arc<dyn OutboundEnqueue>, stored: Bytes) -> Result<(), Error> {
        let Some(list) = t.forward_lists.first() else { return Ok(()) };
        let (local, domain) = list.rsplit_once('@').ok_or_else(|| Error::Invalid("list address".into()))?;
        let mut recipients = t.remote_recipients.clone();
        recipients.sort();
        recipients.dedup();
        let key = {
            let mut h = Sha256::new();
            h.update(list.as_bytes());
            h.update(b"\0");
            h.update(t.mail_from.as_bytes());
            h.update(b"\0");
            h.update(recipients.join(",").as_bytes());
            h.update(b"\0");
            h.update(BlobHash::of(&t.data).as_str().as_bytes());
            hex::encode(h.finalize())
        };
        let blob = self.blobs.put(stored).await?;
        queue
            .enqueue(OutboundMessage {
                blob,
                envelope_from: format!("{local}-bounces@{domain}"),
                recipients,
                sender_domain: domain.to_owned(),
                account: 0,
                submission_key: key,
                bytes: t.data.len() as u64,
            })
            .await
    }

    /// End of DATA: filters, then one idempotent delivery per distinct account.
    async fn deliver(&self, t: &Transaction) -> Reply {
        let Some(_copies) = self.options.budget.try_reserve(EXTRA_COPIES * t.data.len() as u64) else {
            return Reply::new(451, "4.3.1", "Insufficient system resources, try again later");
        };
        if let Some(reply) = self.list_guard(t).await {
            tracing::info!(peer = %t.peer.ip, status = reply.code, "message for a list refused");
            self.trace_inbound(t, "bounced", Some(&reply)).await;
            return reply;
        }
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
            self.trace_inbound(t, "bounced", Some(&reply)).await;
            return reply;
        }
        let folder = match outcome.quarantine {
            Some(QuarantineFolder::Spam) => system_folder("spam"),
            None => system_folder("inbox"),
        };
        // Log-only SPF/DKIM/DMARC: the verdicts go into the header and the log, never into a decision.
        let auth = match &self.options.auth {
            Some(checker) => {
                // DNS can be slow or down: bounded, and a timeout is "no result", never a delayed or refused message.
                let checked = tokio::time::timeout(
                    Duration::from_secs(10),
                    checker.check(&AuthInput { peer: t.peer.ip, helo: &t.helo, mail_from: &t.mail_from, message: &t.data }),
                )
                .await;
                let r = checked.unwrap_or_else(|_| crate::authres::AuthResults::none(&self.hostname));
                tracing::info!(peer = %t.peer.ip, spf = %r.spf, dkim = %r.dkim, dmarc = %r.dmarc, "inbound authentication results");
                Some(r)
            }
            None => None,
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
            authenticated: false,
            auth_results: auth.as_ref().map(|a| a.header_value.as_str()),
        };
        let mut tags = outcome.tags.clone();
        tags.extend(Self::list_tags(&t.forward_lists));
        let stored = Bytes::from(prepare_message_with(&t.data, &trace, &tags));
        let mut seen = HashSet::new();
        let accounts: Vec<AccountId> =
            t.recipients.iter().flat_map(|r| r.accounts.iter().copied()).filter(|a| seen.insert(*a)).collect();
        let mut results = Vec::with_capacity(accounts.len());
        for a in &accounts {
            results.push(self.deliver_one(*a, &t.mail_from, &t.data, stored.clone(), folder).await);
        }
        // The outside members of a list (port 25 has no queue of its own for them): after every local copy is stored.
        if results.iter().all(Result::is_ok) && !t.remote_recipients.is_empty() {
            results.push(match &self.forwarder {
                Some(queue) => self.forward(t, queue, stored.clone()).await,
                None => Err(Error::Busy("forwarding is not available".into())),
            });
        }
        let reply = delivery_reply(results);
        if reply.code < 300 {
            self.trace_inbound(t, "delivered", None).await;
        } else if reply.code >= 500 {
            self.trace_inbound(t, "bounced", Some(&reply)).await;
        }
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

    /// Verify credentials through the `Authenticator`, behind the auth throttle. Failing closed: any
    /// internal error is a temporary failure, never a success.
    async fn authenticate(&self, peer: &PeerInfo, identity: &str, password: &Secret) -> AuthVerdict {
        let temporary = || AuthVerdict::Failure(Reply::new(454, "4.7.0", "Temporary authentication failure"));
        let ip = peer.addr.ip();
        let keys = rusty_svc::throttle_keys(&self.directory, identity).await;
        let mut blocked = false;
        for k in &keys {
            blocked |= matches!(self.rate.check(ip, k).await, Verdict::Blocked { .. });
        }
        if blocked {
            return AuthVerdict::Failure(Reply::new(454, "4.7.0", "Too many failed attempts, try again later"));
        }
        let Some(_slot) = self.rate.try_begin_attempt() else {
            return temporary();
        };
        let address = parse_address(identity).map(|(l, d)| format!("{l}@{d}"));
        let verified = match &address {
            Ok(addr) => self.auth.verify(addr, password.expose()).await,
            Err(_) => Ok(None),
        };
        let service = "submission";
        match (verified, address) {
            (Ok(Some(account)), Ok(address)) => {
                // The plan of the account can leave submission out of the protocols it may use.
                match self.directory.protocol_allowed(account, "submission").await {
                    Ok(true) => {}
                    Ok(false) => return AuthVerdict::Failure(Reply::new(535, "5.7.8", "Authentication credentials invalid")),
                    Err(_) => return temporary(),
                }
                match self.plans.must_change_password(account.0).await {
                    Ok(false) => {}
                    Ok(true) => return AuthVerdict::Failure(Reply::new(535, "5.7.8", "The password must be changed before this account is used")),
                    Err(_) => return temporary(),
                }
                for k in &keys {
                    self.rate.record_success(ip, k).await;
                }
                self.events.emit(ServiceEvent::AuthResult { service, peer: peer.addr, ok: true });
                AuthVerdict::Success(Identity { account, address })
            }
            (Ok(_), _) => {
                for k in &keys {
                    self.rate.record_failure(ip, k).await;
                }
                self.events.emit(ServiceEvent::AuthResult { service, peer: peer.addr, ok: false });
                AuthVerdict::Failure(Reply::new(535, "5.7.8", "Authentication credentials invalid"))
            }
            (Err(_), _) => temporary(),
        }
    }

    /// May `identity` use `address` as the envelope sender? Only its own account address or one of
    /// its aliases (`Directory::resolve` answers `Account(id)` for both).
    async fn check_sender(&self, address: &str, identity: &Identity) -> Result<(), Reply> {
        let refused = || Reply::new(553, "5.7.1", "Sender address rejected: not owned by the authenticated user");
        let Ok((local, domain)) = parse_address(address) else {
            return Err(Reply::new(501, "5.1.7", "Bad address syntax"));
        };
        match self.directory.resolve(&format!("{local}@{domain}")).await {
            Ok(Resolved::Account(id)) if id == identity.account => {
                if let Some(d) = self.submission.as_ref() {
                    let cap = self.messages_per_hour(identity.account).await;
                    if !d.limiter.allow_capped(identity.account, cap).await {
                        return Err(Reply::new(451, "4.7.1", "Sending limit reached, try again later"));
                    }
                }
                Ok(())
            }
            Ok(_) | Err(Error::NotFound(_)) => Err(refused()),
            Err(e) => Err(map_error(&e, ErrorContext::Rcpt)),
        }
    }

    /// End of DATA on submission: the header `From` must be the user too, then local recipients are
    /// delivered and remote ones queued, all durable before the `250`.
    async fn deliver_submitted(&self, t: &Transaction, deps: &SubmissionDeps) -> Reply {
        let Some(_copies) = self.options.budget.try_reserve(EXTRA_COPIES * t.data.len() as u64) else {
            return Reply::new(451, "4.3.1", "Insufficient system resources, try again later");
        };
        let Some(identity) = &t.identity else {
            return Reply::new(530, "5.7.0", "Authentication required");
        };
        if let Some(reply) = self.list_guard(t).await {
            return reply;
        }
        let not_yours = || Reply::new(550, "5.7.1", "The From header is not an address of the authenticated user");
        match from_addresses(&t.data) {
            Ok(list) => {
                for addr in list {
                    match self.directory.resolve(&addr).await {
                        Ok(Resolved::Account(id)) if id == identity.account => {}
                        Ok(_) | Err(Error::NotFound(_)) | Err(Error::Invalid(_)) => return not_yours(),
                        Err(e) => return map_error(&e, ErrorContext::Data),
                    }
                }
            }
            Err(FromError::Missing) => return Reply::new(550, "5.7.1", "A From header is required"),
            Err(FromError::Multiple) => return Reply::new(550, "5.7.1", "Only one From header is allowed"),
            Err(FromError::Unusable) => return not_yours(),
        }
        let now = self.options.clock.now_unix();
        let queue_id = hex12(&BlobHash::of(&t.data));
        let trace = Trace {
            hostname: &self.hostname,
            helo: &t.helo,
            peer: t.peer.ip,
            tls: t.peer.tls,
            mail_from: &t.mail_from,
            queue_id: &queue_id,
            received_unix: now,
            authenticated: true,
            auth_results: None,
        };
        let mut extra = Self::list_tags(&t.forward_lists);
        if !has_header(&t.data, "message-id") {
            extra.push(("Message-ID".to_owned(), generated_message_id(&t.data, &self.hostname)));
        }
        if !has_header(&t.data, "date") {
            extra.push(("Date".to_owned(), rfc5322_date(now)));
        }
        let stored = Bytes::from(prepare_message_with(&t.data, &trace, &extra));
        // The sending budget: counted atomically BEFORE any effect (parallel connections cannot all
        // pass a separate check), restart-safe for remote mail through the queue's own accounting.
        let cap = self.messages_per_hour(identity.account).await;
        if !deps.limiter.try_record_capped(identity.account, cap).await {
            return Reply::new(451, "4.7.1", "Sending limit reached, try again later");
        }
        let mut seen = HashSet::new();
        let accounts: Vec<AccountId> =
            t.recipients.iter().flat_map(|r| r.accounts.iter().copied()).filter(|a| seen.insert(*a)).collect();
        // Effects in an order that converges under a client retry: the local copies first (each
        // idempotent by its delivery id), the remote jobs last, in ONE transaction under a key derived
        // from the submission. A failure before the queue step creates no remote job; a failure at it
        // leaves local copies that the retry finds again by id. Either way the retry ends with every
        // recipient served once. (A single database transaction over both is not possible: mailbox and
        // job repositories are separate seams.)
        let mut results: Vec<Result<(), Error>> = Vec::new();
        for a in &accounts {
            results.push(self.deliver_one(*a, &t.mail_from, &t.data, stored.clone(), system_folder("inbox")).await);
        }
        if results.iter().all(Result::is_ok) && !t.remote_recipients.is_empty() {
            let sender_domain = identity.address.rsplit_once('@').map(|(_, d)| d.to_owned()).unwrap_or_default();
            let mut remote = t.remote_recipients.clone();
            remote.sort();
            let key = {
                let mut h = Sha256::new();
                h.update(identity.account.0.to_be_bytes());
                h.update(t.mail_from.as_bytes());
                h.update(b"\0");
                h.update(remote.join(",").as_bytes());
                h.update(b"\0");
                h.update(BlobHash::of(&t.data).as_str().as_bytes());
                hex::encode(h.finalize())
            };
            results.push(match self.blobs.put(stored.clone()).await {
                Ok(blob) => {
                    deps.outbound
                        .enqueue(OutboundMessage {
                            blob,
                            envelope_from: t.mail_from.clone(),
                            recipients: t.remote_recipients.clone(),
                            sender_domain,
                            account: identity.account.0,
                            submission_key: key,
                            bytes: t.data.len() as u64,
                        })
                        .await
                }
                Err(e) => Err(e),
            });
        }
        let reply = delivery_reply(results);
        tracing::info!(
            peer = %t.peer.ip,
            queue_id = %queue_id,
            bytes = t.data.len(),
            local = accounts.len(),
            remote = t.remote_recipients.len(),
            status = reply.code,
            "submitted message finished"
        );
        reply
    }

    /// Write the batched replies. A peer that does not read them is cut at `limit` (a stalled reader
    /// must not hold a connection slot forever).
    async fn flush(stream: &mut BoxedStream, out: &mut Vec<u8>, limit: Duration) -> bool {
        if out.is_empty() {
            return true;
        }
        let written = tokio::time::timeout(limit, async {
            stream.write_all(out).await?;
            stream.flush().await
        })
        .await;
        out.clear();
        matches!(written, Ok(Ok(())))
    }

    /// Run queued actions. Replies are batched and flushed before anything that blocks.
    async fn execute(
        &self,
        session: &mut Session,
        queue: &mut VecDeque<Action>,
        out: &mut Vec<u8>,
        stream: &mut BoxedStream,
        peer: &PeerInfo,
    ) -> Flow {
        while let Some(action) = queue.pop_front() {
            match action {
                Action::Reply(r) => out.extend_from_slice(&r.render()),
                Action::Close => {
                    Self::flush(stream, out, self.options.write_timeout).await;
                    return Flow::Stop;
                }
                Action::CheckRcpt { address } => {
                    let verdict = self.resolve_rcpt(&address).await;
                    queue.extend(session.answer(Answer::Rcpt(verdict)));
                }
                Action::Authenticate { identity, password } => {
                    let verdict = self.authenticate(peer, &identity, &password).await;
                    queue.extend(session.answer(Answer::Auth(verdict)));
                }
                Action::CheckSender { address, identity } => {
                    let verdict = self.check_sender(&address, &identity).await;
                    queue.extend(session.answer(Answer::Sender(verdict)));
                }
                Action::Deliver(t) => {
                    let reply = match &self.submission {
                        Some(deps) => self.deliver_submitted(&t, deps).await,
                        None => self.deliver(&t).await,
                    };
                    queue.extend(session.answer(Answer::Delivery(reply)));
                }
                Action::StartTls => {
                    if !Self::flush(stream, out, self.options.write_timeout).await {
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
        if Self::flush(stream, out, self.options.write_timeout).await { Flow::Go } else { Flow::Stop }
    }
}

#[async_trait]
impl SessionHandler for InboundHandler {
    async fn handle(&self, mut stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        let (mut session, first) = Session::new(self.session.clone(), PeerFacts { ip: peer.addr.ip(), tls: peer.tls });
        let mut queue: VecDeque<Action> = first.into();
        let mut out = Vec::new();
        let mut buf = vec![0u8; READ_BUF];
        // Progress, not just silence: a client that drips one byte at a time must not hold the slot.
        //  - waiting for a command: the whole line has `command_idle` from the moment the server last
        //    produced something (a reply, a question), however the bytes trickle in;
        //  - during DATA: after a grace of `data_idle`, the transfer must average MIN_DATA_RATE bytes/s.
        let mut unit_start = Instant::now();
        let mut data_bytes: u64 = 0;
        let mut last_phase = session.phase();
        // Memory reserved for the message being received; released when the transfer is over.
        let mut held: Vec<tokio::sync::OwnedSemaphorePermit> = Vec::new();
        let mut reserved: u64 = 0;
        loop {
            if matches!(self.execute(&mut session, &mut queue, &mut out, &mut stream, &peer).await, Flow::Stop)
                || session.phase() == Phase::Closed
            {
                break;
            }
            let phase = session.phase();
            if phase != last_phase {
                last_phase = phase;
                unit_start = Instant::now();
                data_bytes = 0;
                if phase != Phase::Data {
                    held.clear();
                    reserved = 0;
                }
            }
            let in_data = phase == Phase::Data;
            let idle = if in_data { self.options.data_idle } else { self.options.command_idle };
            let wait = if in_data { idle } else { idle.saturating_sub(unit_start.elapsed()) };
            if wait.is_zero() {
                queue.extend(session.timed_out());
                self.execute(&mut session, &mut queue, &mut out, &mut stream, &peer).await;
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    queue.extend(session.shutdown());
                    self.execute(&mut session, &mut queue, &mut out, &mut stream, &peer).await;
                    break;
                }
                read = tokio::time::timeout(wait, stream.read(&mut buf)) => match read {
                    Err(_) => {
                        queue.extend(session.timed_out());
                        self.execute(&mut session, &mut queue, &mut out, &mut stream, &peer).await;
                        break;
                    }
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => {
                        if in_data {
                            data_bytes += n as u64;
                            while reserved < data_bytes {
                                match self.options.budget.try_reserve(RESERVE_STEP) {
                                    Some(p) => {
                                        held.push(p);
                                        reserved += RESERVE_STEP;
                                    }
                                    None => {
                                        tracing::warn!("message memory budget exhausted; refusing data");
                                        queue.extend(session.busy());
                                        break;
                                    }
                                }
                            }
                            if session.phase() == Phase::Closed || reserved < data_bytes {
                                self.execute(&mut session, &mut queue, &mut out, &mut stream, &peer).await;
                                break;
                            }
                            let secs = unit_start.elapsed().saturating_sub(idle).as_secs();
                            if secs > 0 && data_bytes < secs * MIN_DATA_RATE {
                                queue.extend(session.timed_out());
                                self.execute(&mut session, &mut queue, &mut out, &mut stream, &peer).await;
                                break;
                            }
                        }
                        let actions = session.feed(buf.get(..n).unwrap_or_default());
                        if !actions.is_empty() {
                            unit_start = Instant::now();
                        }
                        queue.extend(actions);
                    }
                },
            }
        }
        let _ = stream.shutdown().await;
    }
}
