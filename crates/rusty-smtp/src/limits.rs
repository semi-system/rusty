//! `SubmissionLimiter`: how many messages one account may submit per hour (plan U4 rate limits).
//! A seam like `rusty_svc::RateLimiter`: the in-memory implementation here keeps one sliding window
//! per account; a store- or Valkey-backed limiter with per-tenant keys is another implementation.
//! The clock is injected so tests need no sleeping.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use rusty_core::AccountId;
use rusty_svc::Clock;

pub trait SubmissionLimiter: Send + Sync {
    /// May `account` submit one more message now?
    fn allow(&self, account: AccountId) -> bool;
    /// One message was accepted for `account`.
    fn record(&self, account: AccountId);
}

/// A global budget for the memory messages in flight may hold, in 1 KiB units. Reserving never
/// waits: when the budget is used up the caller refuses new data with a temporary status, so a flood
/// of big transfers degrades into `421`/`451` instead of exhausting the host.
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    sem: Arc<tokio::sync::Semaphore>,
}

impl MemoryBudget {
    pub fn new_mib(mib: u32) -> MemoryBudget {
        MemoryBudget { sem: Arc::new(tokio::sync::Semaphore::new((mib as usize).saturating_mul(1024))) }
    }

    /// Reserve room for `bytes` (rounded up to KiB). `None`: the budget cannot cover it now.
    pub fn try_reserve(&self, bytes: u64) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let kib = u32::try_from(bytes.div_ceil(1024)).ok()?;
        self.sem.clone().try_acquire_many_owned(kib.max(1)).ok()
    }

    /// KiB still free (diagnostics and tests).
    pub fn available_kib(&self) -> usize {
        self.sem.available_permits()
    }
}

/// Most accounts remembered at once; expired windows are dropped first.
const MAX_TRACKED_ACCOUNTS: usize = 100_000;
const WINDOW_MS: u64 = 3_600_000;

pub struct MemorySubmissionLimiter {
    max_per_hour: u32,
    clock: Arc<dyn Clock>,
    sent: Mutex<HashMap<AccountId, VecDeque<u64>>>,
}

impl MemorySubmissionLimiter {
    pub fn new(max_per_hour: u32, clock: Arc<dyn Clock>) -> MemorySubmissionLimiter {
        MemorySubmissionLimiter { max_per_hour, clock, sent: Mutex::new(HashMap::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, VecDeque<u64>>> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expire(window: &mut VecDeque<u64>, now: u64) {
        while window.front().is_some_and(|&t| now.saturating_sub(t) >= WINDOW_MS) {
            window.pop_front();
        }
    }
}

impl SubmissionLimiter for MemorySubmissionLimiter {
    fn allow(&self, account: AccountId) -> bool {
        let now = self.clock.now_ms();
        let mut m = self.lock();
        match m.get_mut(&account) {
            Some(w) => {
                Self::expire(w, now);
                w.len() < self.max_per_hour as usize
            }
            None => self.max_per_hour > 0,
        }
    }

    fn record(&self, account: AccountId) {
        let now = self.clock.now_ms();
        let mut m = self.lock();
        if !m.contains_key(&account) && m.len() >= MAX_TRACKED_ACCOUNTS {
            m.retain(|_, w| {
                Self::expire(w, now);
                !w.is_empty()
            });
        }
        let w = m.entry(account).or_default();
        Self::expire(w, now);
        // Bounded by the cap: never more entries than the limit.
        if w.len() < self.max_per_hour as usize {
            w.push_back(now);
        }
    }
}
