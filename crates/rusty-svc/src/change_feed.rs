//! The process side of the change feed (migration 0017): what happens when another process
//! announces a change, and the service that runs the feed.
//!
//! The feed itself (one `LISTEN` connection) lives in `rusty-store`; this module only sees the
//! `ChangeFeed` / `ChangeSink` traits, so no service crate touches the database driver.

use std::sync::Arc;

use async_trait::async_trait;
use rusty_core::{MailboxId, Result};
use rusty_store::{ChangeFeed, ChangeSink};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::hub::MailboxHub;
use crate::service::Service;

/// Hands what the feed hears to the parts of THIS process that follow changes: the mailbox hub
/// (IMAP IDLE) and, if the process runs the outbound worker, its job waiter.
pub struct ChangeFanout {
    hub: MailboxHub,
    jobs: Option<Arc<Notify>>,
}

impl ChangeFanout {
    pub fn new(hub: MailboxHub) -> ChangeFanout {
        ChangeFanout { hub, jobs: None }
    }

    /// Also wake `jobs` (the outbound worker's wake-up) when a job is queued or after a resync.
    pub fn with_jobs(mut self, jobs: Arc<Notify>) -> ChangeFanout {
        self.jobs = Some(jobs);
        self
    }

    fn wake_jobs(&self) {
        if let Some(jobs) = &self.jobs {
            jobs.notify_one();
        }
    }
}

impl ChangeSink for ChangeFanout {
    fn mailbox_changed(&self, mailbox: MailboxId, seq: i64) {
        self.hub.notify(mailbox, u64::try_from(seq).unwrap_or(0));
    }

    fn jobs_queued(&self) {
        self.wake_jobs();
    }

    fn resync(&self) {
        // Announcements made while the feed was away are gone: every followed mailbox looks at its
        // change log again and the worker looks at the queue.
        self.hub.poke_all();
        self.wake_jobs();
    }

    fn lost(&self) {
        // No detail on purpose (the driver's errors can name the server).
        tracing::warn!("change feed: connection lost, retrying");
    }
}

/// Runs a [`ChangeFeed`] into a [`ChangeSink`] for the life of the process.
pub struct ChangeFeedService {
    feed: Arc<dyn ChangeFeed>,
    sink: Arc<dyn ChangeSink>,
}

impl ChangeFeedService {
    pub fn new(feed: Arc<dyn ChangeFeed>, sink: Arc<dyn ChangeSink>) -> ChangeFeedService {
        ChangeFeedService { feed, sink }
    }
}

#[async_trait]
impl Service for ChangeFeedService {
    fn name(&self) -> &'static str {
        "change-feed"
    }

    async fn run(self: Box<Self>, _ctx: Context, shutdown: CancellationToken) -> Result<()> {
        self.feed.run(self.sink, shutdown).await
    }
}
