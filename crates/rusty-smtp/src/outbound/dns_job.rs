//! The `dns.check` job: "are the records of this domain published?", answered by a service that can query DNS (admin web design, section 7).
//!
//! The admin web does not resolve names itself. `POST /api/v1/domains/{d}/dns/check` queues this job; the worker generates the records the same way
//! `rustyctl domain dns` does (`rusty_core::dns::records_with_facts`), checks them against live DNS (`rusty_core::dns_check`, the same engine as
//! `rustyctl domain check`) and records the observations as drift state (`DnsStore::record_check`), which the web then shows. The check queries other
//! people's name servers: one job checks one domain, and the queue's per-domain cap bounds how often a tenant can ask.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::config::OutboundConfig;
use rusty_core::dns::{records_with_facts, MailFacts};
use rusty_core::dns_check::{check_records, CheckOptions, DnsSource};
use rusty_core::{DomainId, Result};
use rusty_svc::{Context, Service};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// The job kind.
pub const DNS_CHECK_JOB_KIND: &str = rusty_core::config::DNS_CHECK_JOB_KIND;
/// Consecutive failures of a record that was `ok` before an alert is raised.
pub const ALERT_AFTER: u32 = 3;

/// Ask for a check of `domain` (name as stored).
pub async fn enqueue_dns_check(jobs: &rusty_store::JobQueue, id: DomainId, domain: &str) -> Result<()> {
    jobs.enqueue(DNS_CHECK_JOB_KIND, Some(id), &json!({ "domain": domain }), None).await?;
    Ok(())
}

pub struct DnsCheckWorker<S: DnsSource> {
    dns: rusty_store::DnsStore,
    cfg: OutboundConfig,
    mail_host: String,
    source: Arc<S>,
    poll: Duration,
}

impl<S: DnsSource + 'static> DnsCheckWorker<S> {
    pub fn new(dns: rusty_store::DnsStore, cfg: OutboundConfig, mail_host: &str, source: Arc<S>) -> DnsCheckWorker<S> {
        DnsCheckWorker { dns, cfg, mail_host: mail_host.to_owned(), source, poll: Duration::from_secs(2) }
    }

    pub fn with_poll(mut self, poll: Duration) -> DnsCheckWorker<S> {
        self.poll = poll;
        self
    }

    async fn check(&self, id: DomainId, domain: &str) -> Result<()> {
        let stored = self.dns.dkim_keys(id).await?;
        let keys: Vec<(&str, &str, &str)> = stored.iter().map(|k| (k.selector.as_str(), k.algorithm.as_str(), k.public_key_b64.as_str())).collect();
        let relay = self.cfg.relay.as_ref();
        let facts = MailFacts { mail_host: &self.mail_host, public_ips: &self.cfg.public_ips, spf_include: relay.and_then(|r| r.spf_include.as_deref()) };
        let (records, _warnings) = records_with_facts(domain, &facts, &keys)?;
        let opts = CheckOptions { via_relay: relay.is_some(), ..CheckOptions::default() };
        let checks = check_records(self.source.as_ref(), &records, &opts).await;
        // The placeholder for a key that was not generated yet is shown but is no DNS record: it is not recorded.
        let observed: Vec<_> = checks.iter().filter(|c| !c.record.name.starts_with('<')).map(|c| c.observation()).collect();
        self.dns.record_check(id, &observed, ALERT_AFTER).await?;
        Ok(())
    }
}

fn domain_of(payload: &Value) -> Option<&str> {
    payload.get("domain").and_then(Value::as_str).filter(|d| !d.is_empty() && d.len() <= 253)
}

#[async_trait]
impl<S: DnsSource + 'static> Service for DnsCheckWorker<S> {
    fn name(&self) -> &'static str {
        "dns-check"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        tracing::info!("dns check worker started");
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match ctx.jobs.claim("dns-check", &[DNS_CHECK_JOB_KIND], 300).await {
                Ok(Some(job)) => {
                    let outcome = match (job.domain, domain_of(&job.payload)) {
                        (Some(id), Some(name)) => self.check(id, name).await,
                        // Not something we wrote: nothing to check, nothing to retry.
                        _ => Ok(()),
                    };
                    let settled = match outcome {
                        Ok(()) => ctx.jobs.complete(job.id, job.epoch).await,
                        // A fixed text: no detail of the error is kept in the job.
                        Err(_) => ctx.jobs.fail(job.id, job.epoch, "the check could not be recorded", 60).await,
                    };
                    if let Err(e) = settled {
                        tracing::warn!(job = job.id.0, error = %e, "dns check not settled; it will be retried");
                    }
                }
                Ok(None) => {
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.poll) => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dns check queue claim failed");
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.poll.max(Duration::from_secs(1))) => {}
                    }
                }
            }
        }
        tracing::info!("dns check worker stopped");
        Ok(())
    }
}
