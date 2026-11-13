//! Per-mailbox in-process write queue (PG8 decision 2).
//!
//! Every mailbox write takes one permit BEFORE it asks the pool for a connection, so writers of a
//! hot mailbox wait here (cheap, no connection held) instead of each holding a pooled connection
//! while they wait for the Postgres row lock (PG2 measured: pool 4 with 12 hot writers made a cold
//! mailbox 6x slower). The row lock stays the arbiter across processes; this only orders the
//! writers of ONE process.
//!
//! Memory is bounded by construction: a fixed array of [`STRIPES`] semaphores indexed by a hash of
//! the mailbox id, instead of a map that has to be reaped. Two mailboxes that share a stripe are
//! serialised against each other (a bounded extra wait, never a deadlock: a writer holds exactly one
//! permit and takes no second one). The queue is FIFO and cancel-safe: a dropped waiter gives its
//! place up, a permit is released when its guard drops.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::{Error, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Number of stripes. A power of two; 4096 semaphores are a few hundred KiB.
pub const STRIPES: usize = 4096;

/// A permit to write one mailbox. Dropping it lets the next writer in.
#[derive(Debug)]
pub struct MailboxPermit {
    _permit: OwnedSemaphorePermit,
}

pub struct MailboxQueue {
    stripes: Vec<Arc<Semaphore>>,
    wait: Duration,
}

impl MailboxQueue {
    pub fn new(wait: Duration) -> Self {
        MailboxQueue { stripes: (0..STRIPES).map(|_| Arc::new(Semaphore::new(1))).collect(), wait }
    }

    /// The stripe of a mailbox id (splitmix64 finaliser, so sequential ids spread).
    pub fn stripe_of(id: i64) -> usize {
        let mut x = (id as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (x ^ (x >> 31)) as usize % STRIPES
    }

    /// Wait for this mailbox's turn, at most the configured queue timeout; then `Busy` (a fixed
    /// reason): the caller answers a retryable status instead of hanging.
    pub async fn acquire(&self, id: i64) -> Result<MailboxPermit> {
        let Some(sem) = self.stripes.get(Self::stripe_of(id)) else {
            return Err(Error::Busy("mailbox queue unavailable".into()));
        };
        match tokio::time::timeout(self.wait, Arc::clone(sem).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(MailboxPermit { _permit: permit }),
            Ok(Err(_closed)) => Err(Error::Busy("mailbox queue unavailable".into())),
            Err(_elapsed) => Err(Error::Busy("mailbox queue timeout".into())),
        }
    }

    /// Writers currently holding a permit (test and diagnostics).
    pub fn held(&self) -> usize {
        self.stripes.iter().filter(|s| s.available_permits() == 0).count()
    }
}
