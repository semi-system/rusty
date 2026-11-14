//! `MailboxHub`: opens a `Mailbox` for an account through the repository trait and fans out
//! "this mailbox changed" notifications (IMAP IDLE). Notification is a `watch` channel per
//! mailbox carrying the latest change sequence: a notifier never waits, a slow subscriber sees
//! only the newest value, and a channel disappears when its last subscriber is gone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusty_core::{AccountId, MailboxId, Result};
use rusty_store::repo::MailboxRepo;
use rusty_store::{BlobStore, ChangeNotifier, Mailbox, MailboxLimits};
use tokio::sync::watch;

/// Pruning of channels without subscribers starts when this many exist.
const PRUNE_AT: usize = 1024;

type Channels = Arc<Mutex<HashMap<MailboxId, watch::Sender<u64>>>>;

#[derive(Clone)]
pub struct MailboxHub {
    repo: Arc<dyn MailboxRepo>,
    blobs: Arc<dyn BlobStore>,
    limits: MailboxLimits,
    channels: Channels,
}

impl MailboxHub {
    /// `repo` comes from `Repos::mailbox` (any backend); the hub never names a backend.
    pub fn new(repo: Arc<dyn MailboxRepo>, blobs: Arc<dyn BlobStore>, limits: MailboxLimits) -> MailboxHub {
        MailboxHub { repo, blobs, limits, channels: Arc::default() }
    }

    /// The mailbox of `account` (`NotFound` if it has none), with the configured limits.
    pub async fn open_for(&self, account: AccountId) -> Result<Mailbox> {
        let mb = Mailbox::open_with(self.repo.clone(), self.blobs.clone(), account).await?;
        Ok(mb.with_limits(self.limits).with_notifier(Arc::new(self.clone())))
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
