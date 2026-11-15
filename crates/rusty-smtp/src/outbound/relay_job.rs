//! The `relay.check` job: "can this domain's own relay be used?", answered by the outbound service (admin web design, section 14.4).
//!
//! The admin cannot read the relay password back, so it cannot test the connection itself. It enqueues this job instead; the outbound
//! service, which holds the unsealing key, runs the same check as `rustyctl relay check` (resolve, TLS, login, then stop: no mail) through the
//! same route the worker uses, and stores the **fixed category** of the result with its time on the domain's relay row. No server text, host,
//! address or password reaches the database or the job's error.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::config::OutboundConfig;
use rusty_core::{DomainId, Result};
use rusty_svc::{Context, FileSecretSource, Service};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::dial::Dialer;
use super::relay_check::{check_domain_relay, RelayCheckDeps};
use super::route::OutboundRoute;
use super::tls::TlsUpgrader;
use crate::resolve::MxResolver;

/// The job kind.
pub const RELAY_CHECK_JOB_KIND: &str = "relay.check";

/// Ask for a check of the relay of `domain` (name as stored). The result appears on the relay row (`rustyctl domain relay show`).
pub async fn enqueue_relay_check(jobs: &rusty_store::JobQueue, id: DomainId, domain: &str) -> Result<()> {
    jobs.enqueue(RELAY_CHECK_JOB_KIND, Some(id), &json!({ "domain": domain }), None).await?;
    Ok(())
}

pub struct RelayCheckWorker {
    relays: rusty_store::RelayStore,
    route: Arc<dyn OutboundRoute>,
    cfg: OutboundConfig,
    resolver: Arc<dyn MxResolver>,
    dialer: Arc<dyn Dialer>,
    tls: Arc<dyn TlsUpgrader>,
    helo: String,
    poll: Duration,
}

impl RelayCheckWorker {
    pub fn new(
        relays: rusty_store::RelayStore,
        route: Arc<dyn OutboundRoute>,
        cfg: OutboundConfig,
        resolver: Arc<dyn MxResolver>,
        dialer: Arc<dyn Dialer>,
        tls: Arc<dyn TlsUpgrader>,
        helo: &str,
    ) -> RelayCheckWorker {
        let helo = cfg.helo_name.clone().unwrap_or_else(|| helo.to_owned());
        RelayCheckWorker { relays, route, cfg, resolver, dialer, tls, helo, poll: Duration::from_secs(2) }
    }

    /// How long an idle worker sleeps between queue polls.
    pub fn with_poll(mut self, poll: Duration) -> RelayCheckWorker {
        self.poll = poll;
        self
    }

    /// Run the check of one job and record its category. `Ok(())` also for a domain whose relay is gone (nothing to record).
    async fn check(&self, domain_id: DomainId, domain: &str) -> Result<()> {
        let secrets = FileSecretSource::new();
        let deps = RelayCheckDeps { resolver: self.resolver.as_ref(), dialer: self.dialer.as_ref(), tls: self.tls.as_ref(), secrets: &secrets, helo: &self.helo };
        let category = match check_domain_relay(self.route.as_ref(), domain, &self.cfg, &deps).await {
            Ok(_) => "ok",
            Err(f) => f.category(),
        };
        self.relays.record_check(domain_id, category).await?;
        Ok(())
    }
}

fn domain_of(payload: &Value) -> Option<&str> {
    payload.get("domain").and_then(Value::as_str).filter(|d| !d.is_empty() && d.len() <= 253)
}

#[async_trait]
impl Service for RelayCheckWorker {
    fn name(&self) -> &'static str {
        "relay-check"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        tracing::info!("relay check worker started");
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match ctx.jobs.claim("relay-check", &[RELAY_CHECK_JOB_KIND], 120).await {
                Ok(Some(job)) => {
                    let outcome = match (job.domain, domain_of(&job.payload)) {
                        (Some(id), Some(name)) => self.check(id, name).await,
                        // Not something we wrote: nothing to check, nothing to retry.
                        _ => Ok(()),
                    };
                    let settled = match outcome {
                        Ok(()) => ctx.jobs.complete(job.id, job.epoch).await,
                        // A storage failure: the job goes back in the queue (a fixed text; no detail of the error is kept).
                        Err(_) => ctx.jobs.fail(job.id, job.epoch, "the result could not be recorded", 30).await,
                    };
                    if let Err(e) = settled {
                        tracing::warn!(job = job.id.0, error = %e, "relay check not settled; it will be retried");
                    }
                }
                Ok(None) => {
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.poll) => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "relay check queue claim failed");
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(self.poll.max(Duration::from_secs(1))) => {}
                    }
                }
            }
        }
        tracing::info!("relay check worker stopped");
        Ok(())
    }
}
