//! The sender of the webmail (docs/design-webmail.md, sending). The webmail service queues what a person composes (a blob and a row of the outbox); this service, which already is the submission service,
//! takes the row and sends the message through the very same steps as an authenticated SMTP submission, then keeps a copy in the Sent folder. The service parses nothing of the message but the headers the
//! submission path checks, and it answers the person with a fixed word, never a text from a server.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{BlobHash, Flags, Result};
use rusty_directory::Resolved;
use rusty_svc::{Context, Service};
use tokio_util::sync::CancellationToken;

use crate::inbound::{system_folder, InboundHandler, SubmitRefusal};
use crate::session::Identity;

/// A message still `sending` this long after it was asked for was left by a sender that stopped.
const STUCK_AFTER_SECS: i64 = 900;

pub struct WebmailSendService {
    handler: Arc<InboundHandler>,
    poll: Duration,
}

impl WebmailSendService {
    pub fn new(handler: Arc<InboundHandler>) -> WebmailSendService {
        WebmailSendService { handler, poll: Duration::from_secs(2) }
    }

    pub fn with_poll(mut self, poll: Duration) -> WebmailSendService {
        self.poll = poll;
        self
    }

    /// Send the oldest queued message, if any; whether there was one.
    pub async fn run_once(handler: &InboundHandler, ctx: &Context) -> Result<bool> {
        let Some(item) = ctx.outbox.claim().await? else { return Ok(false) };
        let outcome = Self::send(handler, ctx, &item).await;
        match outcome {
            Ok(()) => ctx.outbox.finish(item.id, "sent", None).await?,
            Err(word) => ctx.outbox.finish(item.id, "failed", Some(word)).await?,
        }
        Ok(true)
    }

    async fn send(handler: &InboundHandler, ctx: &Context, item: &rusty_store::OutboxItem) -> std::result::Result<(), &'static str> {
        // The address it is sent from must be the account's own, whatever the row says.
        match ctx.directory.resolve(&item.sender).await {
            Ok(Resolved::Account(id)) if id == item.account => {}
            Ok(_) | Err(rusty_core::Error::NotFound(_) | rusty_core::Error::Invalid(_)) => return Err(SubmitRefusal::NotYours.word()),
            Err(_) => return Err(SubmitRefusal::Busy.word()),
        }
        let hash = BlobHash::parse(&item.blob_hash).map_err(|_| "failed")?;
        let bytes: Bytes = ctx.blobs.get(&hash).await.map_err(|_| "failed")?;
        let identity = Identity { account: item.account, address: item.sender.clone() };
        handler.submit_for(identity, &item.recipients, bytes.to_vec()).await.map_err(SubmitRefusal::word)?;
        // The copy for the Sent folder: a failure here must not make a sent message look unsent.
        match ctx.hub.open_for(item.account).await {
            Ok(mb) => {
                if let Err(e) = mb.append(system_folder("sent"), bytes, Flags::SEEN).await {
                    tracing::warn!(error = %e, "the copy of a sent message could not be kept");
                }
            }
            Err(e) => tracing::warn!(error = %e, "the mailbox of a sent message could not be opened"),
        }
        Ok(())
    }
}

#[async_trait]
impl Service for WebmailSendService {
    fn name(&self) -> &'static str {
        "webmail-send"
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let mut last_sweep = tokio::time::Instant::now();
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match WebmailSendService::run_once(&self.handler, &ctx).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "a message of the webmail outbox could not be sent"),
            }
            if last_sweep.elapsed() > Duration::from_secs(300) {
                last_sweep = tokio::time::Instant::now();
                if let Err(e) = ctx.outbox.fail_stuck(STUCK_AFTER_SECS).await {
                    tracing::warn!(error = %e, "stuck messages of the outbox could not be swept");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(self.poll) => {}
            }
        }
    }
}
