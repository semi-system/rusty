//! The worker of the searches of every mailbox asked for in the admin web (docs/design-admin-search.md). It runs where the mailboxes are (the IMAP service), because the admin service has no access to
//! mail: it takes the oldest queued search from the database, looks in the mailboxes for what it asks and writes at most 100 results (never a body) back for the admin web to read.

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::Result;
use rusty_store::{results_json, MAX_FOUND};
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::service::Service;

/// A search still `running` this long after it was asked for was left by a worker that stopped.
const STUCK_AFTER_SECS: i64 = 600;

pub struct SearchService {
    poll: Duration,
}

impl SearchService {
    pub fn new() -> SearchService {
        SearchService { poll: Duration::from_secs(2) }
    }

    pub fn with_poll(mut self, poll: Duration) -> SearchService {
        self.poll = poll;
        self
    }

    /// Run the oldest queued search, if any; whether there was one.
    pub async fn run_once(ctx: &Context) -> Result<bool> {
        let Some(s) = ctx.searches.claim().await? else { return Ok(false) };
        // A search with nothing to look for would list every message of the platform: refused here as well as at the door.
        let c = &s.criteria;
        let something = c.from_text.is_some() || c.to_text.is_some() || c.subject_text.is_some() || c.since.is_some() || c.until.is_some();
        if !something {
            ctx.searches.finish(s.id, "failed", 0, false, "[]", Some("no_criteria")).await?;
            return Ok(true);
        }
        match ctx.searches.run(c, MAX_FOUND + 1).await {
            Ok(mut rows) => {
                let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_FOUND;
                rows.truncate(usize::try_from(MAX_FOUND).unwrap_or(100));
                let found = i64::try_from(rows.len()).unwrap_or(0);
                ctx.searches.finish(s.id, "done", found, truncated, &results_json(&rows), None).await?;
            }
            // A failure is told as a fixed word: the text of a database error could carry text from a mailbox.
            Err(_) => ctx.searches.finish(s.id, "failed", 0, false, "[]", Some("failed")).await?,
        }
        Ok(true)
    }
}

impl Default for SearchService {
    fn default() -> SearchService {
        SearchService::new()
    }
}

#[async_trait]
impl Service for SearchService {
    fn name(&self) -> &'static str {
        "admin-search"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let mut last_sweep = tokio::time::Instant::now();
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match SearchService::run_once(&ctx).await {
                // One after another without waiting while there is work.
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "a search of the mail could not be run"),
            }
            if last_sweep.elapsed() > Duration::from_secs(300) {
                last_sweep = tokio::time::Instant::now();
                if let Err(e) = ctx.searches.fail_stuck(STUCK_AFTER_SECS).await {
                    tracing::warn!(error = %e, "stuck searches could not be swept");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(self.poll) => {}
            }
        }
    }
}
