//! The sending budget shared over the key-value store: replicas of the submission service give an account one budget, a racing burst never overshoots it, accounts are
//! separate, and a store that is down leaves this process's own window in force.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{AccountId, Result};
use rusty_smtp::limits::{SubmissionLimiter, ValkeySubmissionLimiter};
use rusty_svc::SystemClock;
use rusty_valkey::{Kv, MemoryKv, WindowOutcome};

fn replica(kv: Arc<dyn Kv>, max: u32) -> Arc<ValkeySubmissionLimiter> {
    Arc::new(ValkeySubmissionLimiter::new(kv, max, Arc::new(SystemClock::new())))
}

#[tokio::test]
async fn two_replicas_share_one_budget_per_account() {
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::new());
    let (a, b) = (replica(kv.clone(), 5), replica(kv.clone(), 5));
    // Three on one replica and two on the other use the whole budget of the account; the sixth is refused on either.
    for _ in 0..3 {
        assert!(a.try_record(AccountId(7)).await);
    }
    for _ in 0..2 {
        assert!(b.try_record(AccountId(7)).await);
    }
    assert!(!a.try_record(AccountId(7)).await && !b.try_record(AccountId(7)).await, "the budget is the account's, not the replica's");
    assert!(!a.allow(AccountId(7)).await && !b.allow(AccountId(7)).await);
    // Another account has its own.
    assert!(a.allow(AccountId(8)).await && b.try_record(AccountId(8)).await);
}

#[tokio::test]
async fn a_burst_across_replicas_never_overshoots_the_budget() {
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::new());
    let replicas: Vec<_> = (0..3).map(|_| replica(kv.clone(), 10)).collect();
    let mut tasks = Vec::new();
    for i in 0..90 {
        let r = replicas[i % 3].clone();
        tasks.push(tokio::spawn(async move { r.try_record(AccountId(1)).await }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() {
            ok += 1;
        }
    }
    assert_eq!(ok, 10, "exactly the budget passes, whichever replica a connection lands on");
}

/// A store that is down: every call fails.
struct Down;

#[async_trait]
impl Kv for Down {
    async fn ping(&self) -> Result<()> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn window_try_record(&self, _: &str, _: Duration, _: u32) -> Result<WindowOutcome> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn window_count(&self, _: &str, _: Duration) -> Result<u32> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn counter_incr(&self, _: &str, _: Duration) -> Result<u64> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn counter_get(&self, _: &str) -> Result<u64> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn set(&self, _: &str, _: &[u8], _: Duration) -> Result<()> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn get(&self, _: &str) -> Result<Option<Vec<u8>>> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn touch(&self, _: &str, _: Duration) -> Result<bool> {
        Err(rusty_core::Error::storage("down"))
    }
    async fn delete(&self, _: &str) -> Result<()> {
        Err(rusty_core::Error::storage("down"))
    }
}

#[tokio::test]
async fn a_store_that_is_down_does_not_switch_the_limit_off() {
    let l = replica(Arc::new(Down), 3);
    for _ in 0..3 {
        assert!(l.allow(AccountId(2)).await);
        assert!(l.try_record(AccountId(2)).await);
    }
    assert!(!l.allow(AccountId(2)).await, "the process's own window still limits");
    assert!(!l.try_record(AccountId(2)).await);
}
