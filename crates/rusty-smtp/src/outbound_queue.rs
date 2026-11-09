//! The hand-off of submitted mail for remote recipients to the persistent job queue
//! (`OutboundEnqueue` of `rusty-svc`). Submission replies `250` only after `enqueue` returned, so a
//! job exists in the database whatever happens to the process afterwards. U5 adds the worker that
//! claims `outbound.send` jobs; the payload below is its input.
//!
//! One job per destination domain (independent retries). The body is in the blob store; the job
//! carries its hash. The job's tenant is the SENDER's domain, so the queue's per-domain pending
//! cap applies to the sender.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use rusty_directory::Directory;
use rusty_store::JobQueue;
use rusty_svc::{OutboundEnqueue, OutboundMessage};
use serde_json::json;

/// Job kind the outbound worker claims.
pub const OUTBOUND_JOB_KIND: &str = "outbound.send";

pub struct JobOutbound {
    jobs: JobQueue,
    directory: Directory,
}

impl JobOutbound {
    pub fn new(jobs: JobQueue, directory: Directory) -> JobOutbound {
        JobOutbound { jobs, directory }
    }
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
        let mut by_domain: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for r in &msg.recipients {
            let domain = r.rsplit_once('@').map(|(_, d)| d).ok_or_else(|| Error::Invalid("recipient".into()))?;
            by_domain.entry(domain).or_default().push(r);
        }
        for (domain, rcpts) in by_domain {
            let payload = json!({
                "blob": msg.blob.as_str(),
                "envelope_from": msg.envelope_from,
                "recipients": rcpts,
                "destination": domain,
                "sender_domain": msg.sender_domain,
            });
            self.jobs.enqueue(OUTBOUND_JOB_KIND, Some(tenant.id), &payload, None).await?;
        }
        Ok(())
    }
}
