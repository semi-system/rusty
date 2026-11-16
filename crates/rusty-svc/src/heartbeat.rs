//! The heartbeat of a service (docs/design-admin-monitor.md): every few seconds it tells PostgreSQL that this instance is alive, so the admin web can show which services run, how many
//! replicas of each and how long they have been up. A failing write is logged and tried again at the next beat: a monitor that cannot reach the database never takes a service down.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusty_core::Result;
use rusty_store::HeartbeatRepo;
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::service::Service;

/// How often a service beats. The monitor calls an instance stale after three missed beats.
pub const BEAT_EVERY: Duration = Duration::from_secs(15);

/// The name of this instance: the container's host name (every replica has its own), without anything the operator could not see in `docker ps`.
pub fn instance_name() -> String {
    let raw = std::env::var("HOSTNAME").ok().or_else(|| std::fs::read_to_string("/etc/hostname").ok()).unwrap_or_default();
    let name: String = raw.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')).take(128).collect();
    if name.is_empty() { "unknown".to_owned() } else { name }
}

pub struct HeartbeatService {
    repo: Arc<dyn HeartbeatRepo>,
    service: &'static str,
    instance: String,
    version: String,
    detail: String,
    every: Duration,
    /// Remove the heartbeats nobody has renewed for this long, now and then (the collector does it).
    cleanup_after: Option<Duration>,
}

impl HeartbeatService {
    /// `service` is the service's name (`imap`, `smtp-in`, ...); with a service role the database uses the role's own name whatever is passed here.
    pub fn new(repo: Arc<dyn HeartbeatRepo>, service: &'static str, version: &str) -> HeartbeatService {
        HeartbeatService { repo, service, instance: instance_name(), version: version.chars().take(64).collect(), detail: "{}".to_owned(), every: BEAT_EVERY, cleanup_after: None }
    }

    /// A small JSON object of fixed keys that goes with every beat (at most 2 KiB).
    pub fn with_detail(mut self, detail: String) -> HeartbeatService {
        self.detail = detail;
        self
    }

    pub fn with_every(mut self, every: Duration) -> HeartbeatService {
        self.every = every;
        self
    }

    /// Also remove the rows not renewed for `after` (every twentieth beat): the service that does this needs the grant for it (`rusty_gc`).
    pub fn with_cleanup(mut self, after: Duration) -> HeartbeatService {
        self.cleanup_after = Some(after);
        self
    }
}

#[async_trait]
impl Service for HeartbeatService {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    async fn run(self: Box<Self>, _ctx: Context, shutdown: CancellationToken) -> Result<()> {
        self.run_until(shutdown).await;
        Ok(())
    }
}

impl HeartbeatService {
    /// Beat until `shutdown` is cancelled (the admin web, which has no `Context`, runs this directly).
    pub async fn run_until(&self, shutdown: CancellationToken) {
        let started = Instant::now();
        let mut failing = false;
        let mut beats: u64 = 0;
        loop {
            let uptime = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
            match self.repo.beat(self.service, &self.instance, &self.version, uptime, "ok", &self.detail).await {
                Ok(()) => failing = false,
                // One line when it starts failing, not one every beat.
                Err(e) if !failing => {
                    failing = true;
                    tracing::warn!(service = self.service, error = %e, "the heartbeat could not be written; trying again at the next beat");
                }
                Err(_) => {}
            }
            beats += 1;
            if let Some(after) = self.cleanup_after.filter(|_| beats % 20 == 1) {
                if let Err(e) = self.repo.prune(i64::try_from(after.as_secs()).unwrap_or(i64::MAX)).await {
                    tracing::warn!(service = self.service, error = %e, "the cleanup of old heartbeats failed");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(self.every) => {}
            }
        }
    }
}
