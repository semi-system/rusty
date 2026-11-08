//! `AuthThrottle`: per (peer IP, identity) failure counting with an injected clock, and a
//! process-wide cap on concurrent authentication attempts.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rusty_svc::{AuthThrottle, Clock, RateLimiter, Verdict};

#[derive(Default)]
struct ManualClock(AtomicU64);
impl ManualClock {
    fn advance_secs(&self, s: u64) {
        self.0.fetch_add(s * 1000, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn throttle(clock: Arc<ManualClock>, max: u32, window: u64, concurrent: u32) -> AuthThrottle {
    AuthThrottle::new(max, window, concurrent, clock)
}

// backend-neutral: no database involved, in-memory counters only
#[test]
fn blocks_after_n_failures_for_the_same_peer_and_identity_only() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock.clone(), 3, 60, 4);
    let a = ip("192.0.2.1");
    for _ in 0..3 {
        assert_eq!(t.check(a, "alice@example.org"), Verdict::Allowed);
        t.record_failure(a, "alice@example.org");
    }
    match t.check(a, "alice@example.org") {
        Verdict::Blocked { retry_after_secs } => assert!((1..=60).contains(&retry_after_secs)),
        v => panic!("{v:?}"),
    }
    // identity compare is case-insensitive
    assert!(matches!(t.check(a, "ALICE@example.org"), Verdict::Blocked { .. }));
    // another identity from the same IP, and the same identity from another IP, are separate keys
    assert_eq!(t.check(a, "bob@example.org"), Verdict::Allowed);
    assert_eq!(t.check(ip("192.0.2.2"), "alice@example.org"), Verdict::Allowed);
}

// backend-neutral: no database involved, in-memory counters only
#[test]
fn the_window_expires_and_success_clears() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock.clone(), 2, 60, 4);
    let a = ip("192.0.2.1");
    t.record_failure(a, "x");
    t.record_failure(a, "x");
    assert!(matches!(t.check(a, "x"), Verdict::Blocked { .. }));
    clock.advance_secs(61);
    assert_eq!(t.check(a, "x"), Verdict::Allowed);
    t.record_failure(a, "x");
    t.record_success(a, "x");
    t.record_failure(a, "x");
    assert_eq!(t.check(a, "x"), Verdict::Allowed, "success resets the count");
}

// backend-neutral: no database involved, in-memory counters only
#[test]
fn concurrent_attempts_are_capped_process_wide() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock, 5, 60, 2);
    let p1 = t.try_begin_attempt().expect("first");
    let _p2 = t.try_begin_attempt().expect("second");
    assert!(t.try_begin_attempt().is_none(), "third concurrent attempt must be refused");
    drop(p1);
    assert!(t.try_begin_attempt().is_some(), "a finished attempt frees its slot");
}

// backend-neutral: no database involved, in-memory counters only
#[test]
fn memory_is_bounded_under_an_identity_flood() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock, 5, 60, 2);
    for i in 0..300_000u32 {
        t.record_failure(ip("198.51.100.7"), &format!("user{i}@example.org"));
    }
    assert!(t.tracked() <= 100_000, "tracked {}", t.tracked());
}
