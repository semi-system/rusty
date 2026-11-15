//! The shared authentication throttle: replicas share one counter, a success clears it for all, the keys carry no address of a user, and an unreachable store leaves the
//! process's own counters in force.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::Result;
use rusty_svc::{RateLimiter, SystemClock, ValkeyThrottle, Verdict};
use rusty_valkey::{Kv, MemoryKv, WindowOutcome};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn replica(kv: Arc<dyn Kv>) -> ValkeyThrottle {
    ValkeyThrottle::new(kv, 3, 60, 4, Arc::new(SystemClock::new()))
}

#[tokio::test]
async fn failures_counted_by_one_replica_block_the_others_and_a_success_clears_for_all() {
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::new());
    let (a, b) = (replica(kv.clone()), replica(kv.clone()));
    let peer = ip("192.0.2.7");
    assert_eq!(a.check(peer, "alice@example.org").await, Verdict::Allowed);
    // Two failures at one replica and one at the other: the third blocks both (the in-memory counters alone would need three each).
    a.record_failure(peer, "alice@example.org").await;
    a.record_failure(peer, "ALICE@example.org").await;
    assert_eq!(b.check(peer, "alice@example.org").await, Verdict::Allowed, "two failures are not yet a block");
    b.record_failure(peer, "alice@example.org").await;
    match a.check(peer, "alice@example.org").await {
        Verdict::Blocked { retry_after_secs } => assert_eq!(retry_after_secs, 60, "the whole window: the store does not say what is left"),
        v => panic!("{v:?}"),
    }
    assert!(matches!(b.check(peer, "alice@example.org").await, Verdict::Blocked { .. }));
    // Another identity and another peer are unaffected.
    assert_eq!(a.check(peer, "bob@example.org").await, Verdict::Allowed);
    assert_eq!(a.check(ip("192.0.2.8"), "alice@example.org").await, Verdict::Allowed);
    // A replica that never saw a failure still blocks (it learns it from the store); a success on any replica clears it for all.
    let fresh = replica(kv.clone());
    assert!(matches!(fresh.check(peer, "alice@example.org").await, Verdict::Blocked { .. }));
    fresh.record_success(peer, "alice@example.org").await;
    assert_eq!(b.check(peer, "alice@example.org").await, Verdict::Allowed);
}

#[tokio::test]
async fn the_store_holds_no_readable_address_and_an_ipv6_peer_counts_by_its_64_bit_prefix() {
    let kv = Arc::new(MemoryKv::new());
    let t = replica(kv.clone());
    for low in ["0:0:0:1", "0:0:0:2", "0:0:0:ffff", "1:2:3:4"] {
        t.record_failure(ip(&format!("2001:db8:1:2:{low}")), "alice@example.org").await;
    }
    // Four peers of one /64 share one counter: the next one is blocked at once.
    assert!(matches!(t.check(ip("2001:db8:1:2:9:9:9:9"), "alice@example.org").await, Verdict::Blocked { .. }));
    assert_eq!(t.check(ip("2001:db8:1:3::1"), "alice@example.org").await, Verdict::Allowed, "another /64");
    // The key names are hashes: the address of the user is nowhere in them (the probe asks the store for exactly the key it would use).
    let prefix = "thr:2001:db8:1:2:::";
    let hashed = kv.counter_get(&format!("{prefix}{}", &rusty_core::BlobHash::of(b"alice@example.org").as_str()[..32])).await.unwrap();
    assert!(hashed >= 3, "the counter is under the hashed name: {hashed}");
    assert_eq!(kv.counter_get(&format!("{prefix}alice@example.org")).await.unwrap(), 0, "never under the readable address");
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
async fn an_unreachable_store_does_not_switch_the_throttle_off() {
    let t = replica(Arc::new(Down));
    let peer = ip("192.0.2.50");
    for _ in 0..3 {
        assert_eq!(t.check(peer, "carol@example.org").await, Verdict::Allowed);
        t.record_failure(peer, "carol@example.org").await;
    }
    assert!(matches!(t.check(peer, "carol@example.org").await, Verdict::Blocked { .. }), "the process's own counters still block");
    // The cap on concurrent attempts is local and unaffected.
    let permits: Vec<_> = (0..4).filter_map(|_| t.try_begin_attempt()).collect();
    assert_eq!(permits.len(), 4);
    assert!(t.try_begin_attempt().is_none());
}
