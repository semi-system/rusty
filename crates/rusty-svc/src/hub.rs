//! `MailboxHub`: opens a `Mailbox` for an account through the repository trait and fans out
//! "this mailbox changed" notifications (IMAP IDLE). Notification is a `watch` channel per
//! mailbox carrying the latest change sequence: a notifier never waits, a slow subscriber sees
//! only the newest value, and a channel disappears when its last subscriber is gone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusty_core::{AccountId, MailboxId, Result};
use rusty_directory::Directory;
use rusty_store::repo::MailboxRepo;
use rusty_store::{BlobRouter, ChangeNotifier, IntoRouter, Mailbox, MailboxLimits};
use tokio::sync::watch;

/// Pruning of channels without subscribers starts when this many exist.
const PRUNE_AT: usize = 1024;

type Channels = Arc<Mutex<HashMap<MailboxId, watch::Sender<u64>>>>;

#[derive(Clone)]
pub struct MailboxHub {
    repo: Arc<dyn MailboxRepo>,
    blobs: Arc<dyn BlobRouter>,
    limits: MailboxLimits,
    /// Where an account's plan is read from: the quota and the message size of the plan (docs/design-plans-quota.md). Without it every mailbox has the configured limits only.
    directory: Option<Directory>,
    channels: Channels,
}

impl MailboxHub {
    /// `repo` comes from `Repos::mailbox` (any backend); the hub never names a backend.
    pub fn new(repo: Arc<dyn MailboxRepo>, blobs: impl IntoRouter, limits: MailboxLimits) -> MailboxHub {
        MailboxHub { repo, blobs: blobs.into_router(), limits, directory: None, channels: Arc::default() }
    }

    /// Read the quota and the message size limit of each account from its configuration layers (account, plan, domain, global).
    pub fn with_directory(mut self, directory: Directory) -> MailboxHub {
        self.directory = Some(directory);
        self
    }

    /// The mailbox of `account` (`NotFound` if it has none), with the configured limits.
    pub async fn open_for(&self, account: AccountId) -> Result<Mailbox> {
        let mb = Mailbox::open_with(self.repo.clone(), self.blobs.clone(), account).await?;
        let limits = self.limits_of(account).await?;
        Ok(mb.with_limits(limits).with_notifier(Arc::new(self.clone())))
    }

    /// The configured limits narrowed by the account's plan: `quota_bytes` is the quota, `max_message_bytes` can only lower the configured size limit.
    async fn limits_of(&self, account: AccountId) -> Result<MailboxLimits> {
        let mut limits = self.limits;
        if let Some(dir) = &self.directory {
            let got = dir.effective_configs(account, &["quota_bytes", "max_message_bytes"]).await?;
            if let Some(q) = got.first().and_then(|v| v.as_ref()).and_then(|v| v.as_u64()) {
                limits.quota_bytes = Some(q);
            }
            if let Some(m) = got.get(1).and_then(|v| v.as_ref()).and_then(|v| v.as_u64()) {
                limits.max_message_bytes = limits.max_message_bytes.min(usize::try_from(m).unwrap_or(usize::MAX));
            }
        }
        Ok(limits)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<MailboxId, watch::Sender<u64>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Follow `mailbox`: the receiver holds the latest change sequence announced (0 before any).
    pub fn subscribe(&self, mailbox: MailboxId) -> watch::Receiver<u64> {
        let mut m = self.lock();
        if m.len() >= PRUNE_AT {
            m.retain(|_, tx| tx.receiver_count() > 0);
        }
        m.entry(mailbox).or_insert_with(|| watch::channel(0).0).subscribe()
    }

    /// Announce that `mailbox` changed up to change sequence `seq`. Never blocks and never
    /// fails; without subscribers it is a no-op. Sequences only move forward.
    pub fn notify(&self, mailbox: MailboxId, seq: u64) {
        let mut m = self.lock();
        let Some(tx) = m.get(&mailbox) else {
            return;
        };
        if tx.receiver_count() == 0 {
            m.remove(&mailbox);
            return;
        }
        tx.send_if_modified(|cur| {
            if seq > *cur {
                *cur = seq;
                true
            } else {
                false
            }
        });
    }

    /// Wake every follower of every mailbox without carrying a sequence (`watch` reports a change, the
    /// value stays). Used after the change feed reconnected: announcements made while it was away are
    /// lost, so each IDLE session looks at its change log again. Never lowers or raises a sequence.
    pub fn poke_all(&self) {
        let m = self.lock();
        for tx in m.values() {
            tx.send_modify(|_| {});
        }
    }

    /// Number of live channels (diagnostics and the leak test).
    pub fn channels(&self) -> usize {
        self.lock().len()
    }
}

/// Every mailbox the hub opens announces its committed writes here, so a delivery or an IMAP
/// mutation wakes the IDLE sessions of that mailbox without any call site of its own.
impl ChangeNotifier for MailboxHub {
    fn changed(&self, mailbox: MailboxId, seq: i64) {
        self.notify(mailbox, u64::try_from(seq).unwrap_or(0));
    }
}
