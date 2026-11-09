//! The outbound worker: a `rusty-svc` [`Service`] that claims `outbound.send` jobs from the durable
//! queue, delivers each one to the recipient domain's mail exchangers (or the configured relay), and
//! settles the job.
//!
//! # Guarantees (and their limits)
//!
//! * **Durable.** A job exists before submission replied `250` (U4); the body is a blob. A crash, a
//!   `kill -9` or a lost lease at any point leaves the job claimable again (`JobQueue` lease expiry,
//!   fencing epoch), so mail is never lost by the worker.
//! * **At least once.** SMTP cannot be made exactly-once from the sender's side: if the worker dies after
//!   the remote server accepted the message but before the job was settled, the next run sends it again
//!   and the remote may deliver two copies. This window is as small as one network round trip (the final
//!   reply to DATA). It is documented, tested (`crash_resume`), and the same behaviour every MTA has.
//! * **Idempotent bounces.** A DSN is delivered under an id derived from the job and the failed
//!   recipients, in the same transaction as the job's completion when it is the last step
//!   (`Mailbox::deliver_fenced`), so a crash cannot produce two bounces or a bounce without completion.
//! * **Fencing.** Every irreversible step is preceded by a lease check: before the message is sent
//!   (`Gate`), and the settle operations carry the epoch. A worker whose lease lapsed cannot complete,
//!   reschedule or bounce; its connection is dropped.
//! * **Per-recipient results.** Delivered recipients leave the job, permanently failed ones are
//!   bounced, temporarily failed ones stay for the next attempt (`JobQueue::reschedule` carries the
//!   shrunken list), so a partial success never re-sends to someone who already has the mail.
//! * **No backscatter.** Bounces only for a message a local user submitted; see `dsn`.
//!
//! Retry schedule, expiry and classification are `policy` (pure); the conversation is `client`; DNS is
//! the injected `MxResolver`; sockets are the injected `Dialer`; TLS is the injected `TlsUpgrader`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::config::{OutboundConfig, RelayTls, TlsPolicy};
use rusty_core::{Error, Flags, Result};
use rusty_directory::Resolved;
use rusty_store::{ClaimedJob, JobFence};
use rusty_svc::{Context, Service};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::client::{run_session, Delivery, Gate, HostFailure, Mail, SessionParams, Timeouts};
use super::dial::{forbidden_address, Dialer};
use super::dsn::{self, DsnInput, Failure};
use super::payload::OutboundPayload;
use super::policy::{expired, next_step, Next, Outcome};
use super::route::{OutboundRoute, RelaySpec, Route};
use super::tls::TlsUpgrader;
use crate::dkim::DkimSigner;
use crate::outbound_queue::OUTBOUND_JOB_KIND;
use crate::resolve::{plan_for, to_ascii_domain, MxResolver, Plan, Target};

/// Port of every MX delivery.
pub const SMTP_PORT: u16 = 25;
/// Addresses tried per exchanger.
const MAX_ADDRS_PER_HOST: usize = 4;
/// How long a job waits when its destination is at the concurrency cap.
const BUSY_RETRY_SECS: u32 = 3;

/// Everything the worker talks to, injected.
#[derive(Clone)]
pub struct WorkerDeps {
    pub resolver: Arc<dyn MxResolver>,
    pub dialer: Arc<dyn Dialer>,
    pub tls: Arc<dyn TlsUpgrader>,
    pub route: Arc<dyn OutboundRoute>,
    /// `None`: mail goes unsigned.
    pub signer: Option<Arc<dyn DkimSigner>>,
}

#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub cfg: OutboundConfig,
    /// EHLO name (`helo_name` or the host name) and Reporting-MTA of bounces.
    pub hostname: String,
    /// How long an idle worker sleeps between queue polls (a wake-up from the enqueue side is faster).
    pub poll: Duration,
    /// Name in the lease (`lease_owner`).
    pub name: String,
    /// Port of MX delivery; 25 except in tests.
    pub mx_port: u16,
}

impl WorkerOptions {
    pub fn new(cfg: OutboundConfig, hostname: &str) -> WorkerOptions {
        let hostname = cfg.helo_name.clone().unwrap_or_else(|| hostname.to_owned());
        WorkerOptions { cfg, hostname, poll: Duration::from_secs(2), name: "outbound".to_owned(), mx_port: SMTP_PORT }
    }
}

/// Wakes an idle worker as soon as a job is enqueued in this process.
#[derive(Clone, Default)]
pub struct Wake(pub Arc<Notify>);

pub struct OutboundWorker {
    inner: Arc<Inner>,
    wake: Wake,
}

struct Inner {
    deps: WorkerDeps,
    opts: WorkerOptions,
    domains: DomainLimits,
}

impl OutboundWorker {
    pub fn new(deps: WorkerDeps, opts: WorkerOptions) -> OutboundWorker {
        let per = usize::try_from(opts.cfg.max_concurrent_per_domain).unwrap_or(1).max(1);
        OutboundWorker { inner: Arc::new(Inner { deps, opts, domains: DomainLimits::new(per) }), wake: Wake::default() }
    }

    pub fn with_wake(mut self, wake: Wake) -> OutboundWorker {
        self.wake = wake;
        self
    }
}

/// Per-destination-domain concurrency caps.
struct DomainLimits {
    per: usize,
    map: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl DomainLimits {
    fn new(per: usize) -> DomainLimits {
        DomainLimits { per, map: Mutex::new(HashMap::new()) }
    }

    fn try_acquire(&self, domain: &str) -> Option<OwnedSemaphorePermit> {
        let sem = {
            let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() > 4096 {
                let per = self.per;
                m.retain(|_, s| s.available_permits() < per);
            }
            m.entry(domain.to_owned()).or_insert_with(|| Arc::new(Semaphore::new(self.per))).clone()
        };
        sem.try_acquire_owned().ok()
    }
}

#[async_trait]
impl Service for OutboundWorker {
    fn name(&self) -> &'static str {
        "outbound"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let cfg = &self.inner.opts.cfg;
        let slots = Arc::new(Semaphore::new(usize::try_from(cfg.max_concurrent).unwrap_or(1).max(1)));
        let mut tasks: JoinSet<()> = JoinSet::new();
        tracing::info!(concurrency = cfg.max_concurrent, "outbound worker started");
        loop {
            while tasks.try_join_next().is_some() {}
            let permit = tokio::select! {
                () = shutdown.cancelled() => break,
                p = slots.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
            };
            let claimed = ctx.jobs.claim(&self.inner.opts.name, &[OUTBOUND_JOB_KIND], cfg.lease_secs).await;
            match claimed {
                Ok(Some(job)) => {
                    let inner = self.inner.clone();
                    let (ctx, token) = (ctx.clone(), shutdown.clone());
                    tasks.spawn(async move {
                        let _slot = permit;
                        inner.run_job(ctx, job, token).await;
                    });
                }
                Ok(None) => {
                    drop(permit);
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = self.wake.0.notified() => {}
                        () = tokio::time::sleep(self.inner.opts.poll) => {}
                    }
                }
                Err(e) => {
                    drop(permit);
                    tracing::warn!(error = %e, "outbound queue claim failed");
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.inner.opts.poll.max(Duration::from_secs(1))) => {}
                    }
                }
            }
        }
        // Deliveries see the same token: they release their jobs and finish. Bounded by the caller's grace.
        while tasks.join_next().await.is_some() {}
        tracing::info!("outbound worker stopped");
        Ok(())
    }
}

/// Stops the task when dropped: the heartbeat must die with the delivery that it keeps alive, also when
/// that delivery is cancelled or aborted (a lease that outlives a dead worker would hide the job).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The job as the worker settles it.
struct Settle<'a> {
    ctx: &'a Context,
    job: &'a ClaimedJob,
    fence: JobFence,
}

/// What an attempt (all exchangers) came to.
enum Attempt {
    Delivered(Delivery),
    Failed { permanent: bool, diagnostic: String, status: String, remote: Option<String> },
}

struct LeaseGate {
    ctx: Context,
    fence: JobFence,
}

#[async_trait]
impl Gate for LeaseGate {
    async fn proceed(&self) -> bool {
        self.ctx.jobs.fence_check(self.fence).await.is_ok()
    }
}

impl Inner {
    async fn run_job(&self, ctx: Context, job: ClaimedJob, shutdown: CancellationToken) {
        let fence = JobFence { id: job.id, epoch: job.epoch };
        let payload = match OutboundPayload::decode(&job.payload) {
            Ok(p) => p,
            Err(e) => {
                // Not something we wrote (or corrupted): nothing can be delivered or bounced from it.
                tracing::error!(job = job.id.0, error = %e, "undecodable outbound job; dropping it");
                let _ = ctx.jobs.complete(job.id, job.epoch).await;
                return;
            }
        };
        // The lease is renewed while the delivery runs; losing it cancels the delivery.
        let token = shutdown.child_token();
        let lost = Arc::new(AtomicBool::new(false));
        let _heartbeat = AbortOnDrop({
            let (jobs, token, lost) = (ctx.jobs.clone(), token.clone(), lost.clone());
            let lease = self.opts.cfg.lease_secs;
            let every = Duration::from_secs(u64::from((lease / 3).max(1)));
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = token.cancelled() => return,
                        () = tokio::time::sleep(every) => {}
                    }
                    if jobs.heartbeat(fence.id, fence.epoch, lease).await.is_err() {
                        lost.store(true, Ordering::SeqCst);
                        token.cancel();
                        return;
                    }
                }
            })
        });
        let outcome = self.deliver_job(&ctx, &job, fence, payload.clone(), &token).await;
        if lost.load(Ordering::SeqCst) {
            tracing::warn!(job = job.id.0, "lease lost; another worker owns the job now");
            return;
        }
        if let Err(e) = outcome {
            // A settle step failed (database trouble): the lease lapses and the job runs again.
            tracing::warn!(job = job.id.0, error = %e, "outbound job not settled; it will be retried");
        }
    }

    async fn deliver_job(&self, ctx: &Context, job: &ClaimedJob, fence: JobFence, mut payload: OutboundPayload, token: &CancellationToken) -> Result<()> {
        let settle = Settle { ctx, job, fence };
        let cfg = &self.opts.cfg;
        let now = ctx.jobs.now().await?;
        let original = match ctx.blobs.get(&payload.blob).await {
            Ok(b) => b,
            Err(Error::NotFound(_)) => {
                tracing::error!(job = job.id.0, "the message body is gone from the blob store");
                let failures = self.failures_for(&payload, "the message body is no longer available", "5.3.0", None, &payload.recipients);
                return self.bounce_and_finish(&settle, &payload, &Bytes::new(), failures, now).await;
            }
            Err(e) => return Err(e),
        };

        if expired(cfg, job.created_at, now) {
            let failures = self.failures_for(&payload, "delivery attempts for this message continued for too long and were abandoned", "4.4.7", None, &payload.recipients);
            return self.bounce_and_finish(&settle, &payload, &original, failures, now).await;
        }
        let Some(_domain_slot) = self.domains.try_acquire(&payload.destination) else {
            // Another delivery to this domain holds the cap: try again shortly without spending a try.
            return ctx.jobs.reschedule(job.id, job.epoch, &payload.encode(), "destination busy", BUSY_RETRY_SECS).await;
        };

        let message = match self.signed(&payload, &original) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(job = job.id.0, error = %e, "DKIM signing failed; deferring the message");
                return self.retry_or_bounce(&settle, &mut payload, &original, Vec::new(), "DKIM signing is misconfigured".into(), now).await;
            }
        };
        let gate = LeaseGate { ctx: ctx.clone(), fence };
        let attempt = tokio::select! {
            () = token.cancelled() => None,
            a = self.attempt(&payload, &message, &gate, token) => Some(a),
        };
        let Some(attempt) = attempt else {
            // Shutting down (the lost-lease case returned before this): hand the job back now.
            let _ = ctx.jobs.reschedule(job.id, job.epoch, &payload.encode(), "shutting down", 0).await;
            return Ok(());
        };

        match attempt {
            Attempt::Delivered(delivery) => {
                let mut failures = Vec::new();
                let mut pending = Vec::new();
                let mut last_temp = String::new();
                for r in delivery.results {
                    match r.outcome {
                        Outcome::Delivered => {
                            tracing::info!(job = job.id.0, remote = %delivery.remote, tls = ?delivery.tls.as_ref().map(|t| (t.verified, t.protocol.clone())), "recipient accepted");
                        }
                        Outcome::Temporary { diagnostic } => {
                            last_temp = diagnostic;
                            pending.push(r.recipient);
                        }
                        Outcome::Permanent { diagnostic, status } => {
                            failures.push(Failure { recipient: r.recipient, status, diagnostic, remote_mta: Some(delivery.remote.clone()) });
                        }
                    }
                }
                payload.recipients = pending.clone();
                self.settle(&settle, &mut payload, &original, failures, last_temp, now).await
            }
            Attempt::Failed { permanent: true, diagnostic, status, remote } => {
                let failures = self.failures_for(&payload, &diagnostic, &status, remote, &payload.recipients);
                payload.recipients.clear();
                self.settle(&settle, &mut payload, &original, failures, String::new(), now).await
            }
            Attempt::Failed { permanent: false, diagnostic, .. } => {
                self.settle(&settle, &mut payload, &original, Vec::new(), diagnostic, now).await
            }
        }
    }

    fn failures_for(&self, _p: &OutboundPayload, diagnostic: &str, status: &str, remote: Option<String>, rcpts: &[String]) -> Vec<Failure> {
        rcpts
            .iter()
            .map(|r| Failure { recipient: r.clone(), status: status.to_owned(), diagnostic: diagnostic.to_owned(), remote_mta: remote.clone() })
            .collect()
    }

    /// DKIM-sign when a key exists for the domain of the header `From` (alignment) and it is the
    /// sender's tenant; otherwise the message goes unsigned.
    fn signed(&self, payload: &OutboundPayload, stored: &Bytes) -> Result<Bytes> {
        let original = &Bytes::from(crate::message::outbound_copy(stored));
        let Some(signer) = &self.deps.signer else { return Ok(original.clone()) };
        let from_domain = crate::message::from_addresses(original)
            .ok()
            .and_then(|a| if a.len() == 1 { a.into_iter().next() } else { None })
            .and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()));
        let Some(domain) = from_domain else { return Ok(original.clone()) };
        if domain != payload.sender_domain {
            tracing::warn!("the From domain is not the sender's tenant domain; sending unsigned");
            return Ok(original.clone());
        }
        let sig = signer.sign(&domain, original)?;
        if sig.is_empty() {
            return Ok(original.clone());
        }
        let mut out = sig;
        out.extend_from_slice(original);
        Ok(Bytes::from(out))
    }

    // ------------------------------------------------------------------ one attempt

    async fn attempt(&self, p: &OutboundPayload, message: &[u8], gate: &dyn Gate, token: &CancellationToken) -> Attempt {
        let cfg = &self.opts.cfg;
        let route = match self.deps.route.route(&p.sender_domain, &p.destination).await {
            Ok(r) => r,
            Err(e) => return temp_failure(format!("routing failed: {e}")),
        };
        let (targets, relay, bind, port, direct): (Vec<Target>, Option<RelaySpec>, Option<std::net::IpAddr>, u16, bool) = match route {
            Route::Direct { bind } => {
                let Some(domain) = to_ascii_domain(&p.destination) else {
                    return perm_failure("the destination domain name is invalid".into(), "5.1.2".into());
                };
                let answer = match self.deps.resolver.mx(&domain).await {
                    Ok(a) => a,
                    Err(_) => return temp_failure("DNS lookup of the mail exchangers failed".into()),
                };
                match plan_for(&domain, answer, p.blob.as_str().bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b)))) {
                    Plan::Permanent(why) => return perm_failure(why, "5.1.2".into()),
                    Plan::Try(t) => (t, None, bind, self.opts.mx_port, true),
                }
            }
            Route::Relay(r) => (vec![Target { host: r.host.clone(), preference: 0 }], Some(r), None, 0, false),
        };
        let timeouts = Timeouts { command: Duration::from_secs(cfg.command_timeout_secs), data: Duration::from_secs(cfg.data_timeout_secs) };
        let connect = Duration::from_secs(cfg.connect_timeout_secs);
        let mut last_temp: Option<String> = None;
        for target in targets {
            if token.is_cancelled() {
                return temp_failure("shutting down".into());
            }
            let addrs = if let Ok(ip) = target.host.parse::<std::net::IpAddr>() {
                vec![ip]
            } else {
                match self.deps.resolver.addresses(&target.host).await {
                    Ok(a) => a,
                    Err(_) => {
                        last_temp = Some(format!("DNS lookup of {} failed", target.host));
                        continue;
                    }
                }
            };
            let addrs: Vec<_> = addrs.into_iter().filter(|a| !direct || !forbidden_address(*a)).take(MAX_ADDRS_PER_HOST).collect();
            if addrs.is_empty() {
                last_temp = Some(format!("{} has no usable address", target.host));
                continue;
            }
            let (policy, implicit, dial_port) = match &relay {
                Some(r) => (
                    if r.tls == RelayTls::None { TlsPolicy::None } else { TlsPolicy::Verified },
                    r.tls == RelayTls::Implicit,
                    r.port,
                ),
                None => (cfg.tls_for(&p.destination), false, port),
            };
            let auth = relay.as_ref().and_then(|r| r.credentials.as_ref()).map(|c| (c.username.as_str(), c.password.as_str()));
            for ip in addrs {
                let mut policy_now = policy;
                loop {
                    let stream = match self.deps.dialer.dial(ip, dial_port, bind, connect).await {
                        Ok(s) => s,
                        Err(e) => {
                            last_temp = Some(format!("cannot connect to {}: {}", target.host, e.kind()));
                            break;
                        }
                    };
                    let params = SessionParams { helo: &self.opts.hostname, server_name: &target.host, policy: policy_now, implicit_tls: implicit, auth, timeouts };
                    let mail = Mail { from: &p.envelope_from, recipients: &p.recipients, data: message };
                    match run_session(stream, &params, self.deps.tls.as_ref(), &mail, gate, token).await {
                        Ok(delivery) => return Attempt::Delivered(delivery),
                        Err(HostFailure::TlsHandshake(why)) => {
                            tracing::info!(host = %target.host, reason = %why, "TLS handshake failed");
                            if policy_now == TlsPolicy::Opportunistic {
                                // Opportunistic means exactly this: fall back to plain text.
                                policy_now = TlsPolicy::None;
                                continue;
                            }
                            last_temp = Some(format!("TLS to {} failed: {why}", target.host));
                            break;
                        }
                        Err(HostFailure::Temporary(why)) => {
                            tracing::info!(host = %target.host, %ip, reason = %why, "delivery attempt deferred");
                            last_temp = Some(format!("{}: {why}", target.host));
                            break;
                        }
                        Err(HostFailure::Permanent { diagnostic, status }) => {
                            return Attempt::Failed { permanent: true, diagnostic, status, remote: Some(target.host.clone()) };
                        }
                    }
                }
            }
        }
        temp_failure(last_temp.unwrap_or_else(|| "no mail exchanger could be reached".into()))
    }

    // ------------------------------------------------------------------ settling

    /// After an attempt: `payload.recipients` holds what is still pending, `failures` what failed for good.
    async fn settle(&self, s: &Settle<'_>, payload: &mut OutboundPayload, original: &Bytes, failures: Vec<Failure>, last_temp: String, now: i64) -> Result<()> {
        if payload.recipients.is_empty() {
            return self.bounce_and_finish(s, payload, original, failures, now).await;
        }
        self.retry_or_bounce(s, payload, original, failures, last_temp, now).await
    }

    /// Recipients remain: bounce the permanent failures now (idempotently), then either schedule the
    /// next try or, when the message is too old, bounce the rest too.
    async fn retry_or_bounce(&self, s: &Settle<'_>, payload: &mut OutboundPayload, original: &Bytes, mut failures: Vec<Failure>, last_temp: String, now: i64) -> Result<()> {
        let cfg = &self.opts.cfg;
        payload.tries = payload.tries.saturating_add(1);
        match next_step(cfg, s.job.created_at, now, payload.tries.saturating_sub(1)) {
            Next::GiveUp => {
                let why = if last_temp.is_empty() { "delivery attempts continued for too long".to_owned() } else { last_temp };
                failures.extend(self.failures_for(payload, &why, "4.4.7", None, &payload.recipients));
                payload.recipients.clear();
                self.bounce_and_finish(s, payload, original, failures, now).await
            }
            Next::Retry { after_secs } => {
                if !failures.is_empty() {
                    // Unfenced but idempotent: the same failures of this job always bounce once.
                    self.deliver_dsn(s, payload, original, &failures, now, false).await?;
                }
                tracing::info!(job = s.job.id.0, pending = payload.recipients.len(), after_secs, "delivery deferred");
                s.ctx.jobs.reschedule(s.job.id, s.job.epoch, &payload.encode(), &last_temp, after_secs).await
            }
        }
    }

    /// Nothing is left to try: bounce the failures (if any, and if a bounce is allowed) and finish the
    /// job, in one transaction when there is a bounce to store.
    async fn bounce_and_finish(&self, s: &Settle<'_>, payload: &OutboundPayload, original: &Bytes, failures: Vec<Failure>, now: i64) -> Result<()> {
        if failures.is_empty() {
            return s.ctx.jobs.complete(s.job.id, s.job.epoch).await;
        }
        match self.deliver_dsn(s, payload, original, &failures, now, true).await {
            Ok(true) => Ok(()),
            Ok(false) => s.ctx.jobs.complete(s.job.id, s.job.epoch).await,
            Err(e) => Err(e),
        }
    }

    /// Store the bounce in the sender's Inbox. `Ok(true)`: stored (and, when `finish`, the job completed
    /// in the same transaction); `Ok(false)`: no bounce is allowed or possible (null sender, not a local
    /// account); the caller completes the job.
    async fn deliver_dsn(&self, s: &Settle<'_>, payload: &OutboundPayload, original: &Bytes, failures: &[Failure], now: i64, finish: bool) -> Result<bool> {
        if payload.envelope_from.is_empty() {
            tracing::info!(job = s.job.id.0, "no bounce for a null-sender message");
            return Ok(false);
        }
        // Only for a LOCAL account of the sender's tenant: never to a foreign or forged address.
        let account = match s.ctx.directory.resolve(&payload.envelope_from).await {
            Ok(Resolved::Account(a)) => a,
            _ => {
                tracing::warn!(job = s.job.id.0, "the sender is not a local account; no bounce is generated");
                return Ok(false);
            }
        };
        let key = s.job.id.0.to_string();
        let bytes = dsn::build(&DsnInput {
            hostname: &self.opts.hostname,
            to: &payload.envelope_from,
            failures,
            original,
            arrival: s.job.created_at,
            now,
            job_key: &key,
        });
        let mailbox = s.ctx.hub.open_for(account).await?;
        let folder = crate::inbound::system_folder("inbox");
        let id = dsn::delivery_id(&key, failures);
        let raw = Bytes::from(bytes);
        let result = if finish {
            mailbox.deliver_fenced(folder, raw, Flags::default(), id, s.fence).await
        } else {
            mailbox.deliver(folder, raw, Flags::default(), id).await
        };
        match result {
            Ok(_) => Ok(true),
            // Same id, different bytes (the Date moved on): already stored. For the fenced form a
            // Conflict may also mean a stale fence; ask.
            Err(Error::Conflict(_)) => {
                if finish {
                    if s.ctx.jobs.fence_check(s.fence).await.is_ok() {
                        s.ctx.jobs.complete(s.job.id, s.job.epoch).await?;
                        Ok(true)
                    } else {
                        Err(Error::Conflict("stale job epoch".into()))
                    }
                } else {
                    Ok(true)
                }
            }
            // The job's tenant rule refuses a mailbox of another domain: not ours to bounce into.
            Err(Error::Invalid(_)) => {
                tracing::warn!(job = s.job.id.0, "the bounce target is outside the job's tenant; not delivered");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

fn temp_failure(diagnostic: String) -> Attempt {
    Attempt::Failed { permanent: false, diagnostic, status: "4.4.1".into(), remote: None }
}

fn perm_failure(diagnostic: String, status: String) -> Attempt {
    Attempt::Failed { permanent: true, diagnostic, status, remote: None }
}
