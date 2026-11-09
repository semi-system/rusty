//! The hand-off of submitted mail for remote recipients to the persistent job queue
//! (`OutboundEnqueue` of `rusty-svc`). Submission replies `250` only after `enqueue` returned, so a
//! job exists in the database whatever happens to the process afterwards. The worker
//! (`outbound::worker`) claims `outbound.send` jobs; the payload below is its input.
//!
//! One job per destination domain (independent retries). The body is in the blob store; the job
//! carries its hash. The job's tenant is the SENDER's domain, so the queue's per-domain pending
//! cap applies to the sender's tenant, and the submitting ACCOUNT is the job's owner, so one user
//! cannot use up the tenant's queue (per-account pending cap) or exceed the submission budget
//! (jobs, recipients and bytes per window; the check-and-record is part of the enqueue transaction,
//! so it is atomic under parallel connections and survives a restart).
//!
//! All the jobs of one submission are created in ONE transaction, each under an idempotency key
//! derived from the submission, so a client retry after a temporary failure converges on the same
//! jobs instead of queueing the message twice. A full queue or an exhausted budget is `Busy` (a
//! temporary SMTP failure: the mail is not bounced, the client retries later).

use std::collections::BTreeMap;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use rusty_directory::Directory;
use rusty_store::{BatchSpec, JobQueue, OwnerLimits};
use rusty_svc::{OutboundEnqueue, OutboundMessage};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::outbound::worker::Wake;

/// Job kind the outbound worker claims.
pub const OUTBOUND_JOB_KIND: &str = "outbound.send";

/// What one account may have in the queue and submit per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitLimits {
    pub max_pending: u32,
    pub window_secs: i64,
    pub max_jobs: u32,
    pub max_recipients: u64,
    pub max_bytes: u64,
}

impl SubmitLimits {
    pub fn from_config(c: &rusty_core::config::OutboundConfig) -> SubmitLimits {
        SubmitLimits {
            max_pending: c.max_pending_per_account,
            window_secs: i64::try_from(c.submit_window_secs).unwrap_or(3600),
            max_jobs: c.submit_max_jobs,
            max_recipients: c.submit_max_recipients,
            max_bytes: c.submit_max_bytes,
        }
    }
}

impl Default for SubmitLimits {
    fn default() -> SubmitLimits {
        SubmitLimits::from_config(&rusty_core::config::OutboundConfig::default())
    }
}

pub struct JobOutbound {
    jobs: JobQueue,
    directory: Directory,
    limits: SubmitLimits,
    wake: Option<Wake>,
}

impl JobOutbound {
    pub fn new(jobs: JobQueue, directory: Directory) -> JobOutbound {
        JobOutbound { jobs, directory, limits: SubmitLimits::default(), wake: None }
    }

    pub fn with_limits(mut self, limits: SubmitLimits) -> JobOutbound {
        self.limits = limits;
        self
    }

    /// Wake an idle worker in this process as soon as jobs are queued.
    pub fn with_wake(mut self, wake: Wake) -> JobOutbound {
        self.wake = Some(wake);
        self
    }
}

fn job_key(submission: &str, destination: &str) -> Option<String> {
    if submission.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(submission.as_bytes());
    h.update(b"\0");
    h.update(destination.as_bytes());
    Some(hex::encode(h.finalize()))
}

#[async_trait]
impl OutboundEnqueue for JobOutbound {
    async fn enqueue(&self, msg: OutboundMessage) -> Result<()> {
        if msg.recipients.is_empty() {
            return Err(Error::Invalid("no recipients".into()));
        }
        let tenant = self
            .directory
            .find_domain(&msg.sender_domain)
            .await?
            .ok_or_else(|| Error::NotFound("sender domain".into()))?;
        let mut by_domain: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for r in &msg.recipients {
            let domain = r.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).ok_or_else(|| Error::Invalid("recipient".into()))?;
            by_domain.entry(domain).or_default().push(r);
        }
        let payloads: Vec<(serde_json::Value, Option<String>, u32)> = by_domain
            .into_iter()
            .map(|(domain, rcpts)| {
                let payload = json!({
                    "blob": msg.blob.as_str(),
                    "envelope_from": msg.envelope_from,
                    "recipients": rcpts,
                    "destination": domain,
                    "sender_domain": msg.sender_domain,
                });
                let n = u32::try_from(rcpts.len()).unwrap_or(u32::MAX);
                (payload, job_key(&msg.submission_key, &domain), n)
            })
            .collect();
        let specs: Vec<BatchSpec<'_>> = payloads
            .iter()
            .map(|(p, key, n)| BatchSpec {
                kind: OUTBOUND_JOB_KIND,
                domain: Some(tenant.id),
                payload: p,
                dedupe_key: key.as_deref(),
                units: *n,
                bytes: msg.bytes,
                max_attempts: 20,
            })
            .collect();
        let l = &self.limits;
        let owner = OwnerLimits {
            max_pending: l.max_pending,
            window_secs: l.window_secs,
            max_jobs: l.max_jobs,
            max_units: l.max_recipients,
            max_bytes: l.max_bytes,
        };
        match self.jobs.enqueue_batch(Some((msg.account, owner)), &specs).await {
            Ok(_) => {
                if let Some(w) = &self.wake {
                    w.0.notify_one();
                }
                Ok(())
            }
            // A full queue or an exhausted budget passes with time: temporary, never a bounce.
            Err(Error::Quota(_)) => Err(Error::Busy("the outbound queue is full or the submission limit is reached".into())),
            Err(e) => Err(e),
        }
    }
}
