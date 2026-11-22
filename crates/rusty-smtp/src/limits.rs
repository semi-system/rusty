//! `SubmissionLimiter`: how many messages one account may submit per hour (plan U4 rate limits).
//! A seam like `rusty_svc::RateLimiter`: the in-memory implementation here keeps one sliding window
//! per account; a store- or Valkey-backed limiter with per-tenant keys is another implementation.
//! The clock is injected so tests need no sleeping.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use rusty_core::AccountId;
use rusty_svc::Clock;

#[async_trait::async_trait]
pub trait SubmissionLimiter: Send + Sync {
    /// May `account` submit one more message now?
    async fn allow(&self, account: AccountId) -> bool;
    /// Count one message for `account` if the budget allows it, in ONE atomic step (parallel
    /// connections cannot all pass a separate check). `false`: over the limit, nothing counted.
    async fn try_record(&self, account: AccountId) -> bool;
    /// [`allow`](Self::allow) with a budget of `cap` messages an hour in place of the configured one (the plan of the account says how many it may send).
    async fn allow_capped(&self, account: AccountId, cap: u32) -> bool {
        let _ = cap;
        self.allow(account).await
    }
    /// [`try_record`](Self::try_record) with a budget of `cap` messages an hour.
    async fn try_record_capped(&self, account: AccountId, cap: u32) -> bool {
        let _ = cap;
        self.try_record(account).await
    }
}

/// The shared in-flight memory budget now lives in rusty-svc (IMAP APPEND draws on it too).
pub use rusty_svc::MemoryBudget;

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

#[async_trait::async_trait]
impl SubmissionLimiter for MemorySubmissionLimiter {
    async fn allow(&self, account: AccountId) -> bool {
        self.allow_capped(account, self.max_per_hour).await
    }

    async fn try_record(&self, account: AccountId) -> bool {
        self.try_record_capped(account, self.max_per_hour).await
    }

    async fn allow_capped(&self, account: AccountId, cap: u32) -> bool {
        let now = self.clock.now_ms();
        let mut m = self.lock();
        match m.get_mut(&account) {
            Some(w) => {
                Self::expire(w, now);
                w.len() < cap as usize
            }
            None => cap > 0,
        }
    }

    async fn try_record_capped(&self, account: AccountId, cap: u32) -> bool {
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
        if w.len() < cap as usize {
            w.push_back(now);
            true
        } else {
            false
        }
    }
}

/// The sending budget shared by every instance of the submission service, kept in the key-value store of `[valkey]`: one sliding window of an hour per account (the store's own clock,
/// so replicas with skewed clocks agree), counted atomically. N replicas give the account one budget, not N.
///
/// The in-process limiter runs alongside as the floor: a store that cannot be reached must not switch the limit off, so an unreachable store is logged and this process's own window decides.
/// The key holds the account number only, nothing readable.
pub struct ValkeySubmissionLimiter {
    kv: Arc<dyn rusty_valkey::Kv>,
    local: MemorySubmissionLimiter,
    max_per_hour: u32,
}

impl ValkeySubmissionLimiter {
    pub fn new(kv: Arc<dyn rusty_valkey::Kv>, max_per_hour: u32, clock: Arc<dyn Clock>) -> ValkeySubmissionLimiter {
        ValkeySubmissionLimiter { kv, local: MemorySubmissionLimiter::new(max_per_hour, clock), max_per_hour }
    }

    fn key(account: AccountId) -> String {
        format!("sub:{}", account.0)
    }

    const WINDOW: std::time::Duration = std::time::Duration::from_millis(WINDOW_MS);
}

#[async_trait::async_trait]
impl SubmissionLimiter for ValkeySubmissionLimiter {
    async fn allow(&self, account: AccountId) -> bool {
        self.allow_capped(account, self.max_per_hour).await
    }

    async fn try_record(&self, account: AccountId) -> bool {
        self.try_record_capped(account, self.max_per_hour).await
    }

    async fn allow_capped(&self, account: AccountId, cap: u32) -> bool {
        match self.kv.window_count(&Self::key(account), Self::WINDOW).await {
            Ok(n) => n < cap,
            Err(e) => {
                tracing::warn!(error = %e, "the shared sending budget is unreachable; this process's own window decides");
                self.local.allow_capped(account, cap).await
            }
        }
    }

    async fn try_record_capped(&self, account: AccountId, cap: u32) -> bool {
        match self.kv.window_try_record(&Self::key(account), Self::WINDOW, cap).await {
            Ok(out) => {
                if out.allowed {
                    // Counted in the process too, so the floor stays in step while the store is reachable.
                    self.local.try_record_capped(account, cap).await;
                }
                out.allowed
            }
            Err(e) => {
                tracing::warn!(error = %e, "the shared sending budget is unreachable; this process's own window decides");
                self.local.try_record_capped(account, cap).await
            }
        }
    }
}
