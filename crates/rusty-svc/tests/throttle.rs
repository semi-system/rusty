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

#[test]
fn memory_is_bounded_under_an_identity_flood() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock, 5, 60, 2);
    for i in 0..300_000u32 {
        t.record_failure(ip("198.51.100.7"), &format!("user{i}@example.org"));
    }
    assert!(t.tracked() <= 100_000, "tracked {}", t.tracked());
}

#[test]
fn every_spelling_of_an_identity_shares_one_counter() {
    // Review I1: whitespace, a trailing dot, case and IDN spellings are the same account to the
    // directory, so they must be one key here (else each spelling is a fresh guessing budget).
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock, 3, 60, 4);
    let a = ip("192.0.2.1");
    for _ in 0..3 {
        t.record_failure(a, "alice@a.test");
    }
    for spelling in [
        "alice@a.test",
        " alice@a.test",
        "  alice@a.test",
        "alice@a.test ",
        "alice@a.test.",
        "ALICE@A.TEST",
        "\talice@a.test\r",
    ] {
        assert!(matches!(t.check(a, spelling), Verdict::Blocked { .. }), "{spelling:?} escaped the throttle");
    }
    // Unicode / IDN spellings of one domain collapse too.
    for _ in 0..3 {
        t.record_failure(a, "bob@b\u{fc}cher.example");
    }
    for spelling in ["bob@xn--bcher-kva.example", "BOB@B\u{dc}CHER.example", " bob@b\u{fc}cher.example."] {
        assert!(matches!(t.check(a, spelling), Verdict::Blocked { .. }), "{spelling:?} escaped the throttle");
    }
    // A different account is untouched, and so is garbage (kept as its own trimmed key).
    assert_eq!(t.check(a, "carol@a.test"), Verdict::Allowed);
    assert_eq!(t.check(a, "not an address"), Verdict::Allowed);
}

#[test]
fn an_ipv6_peer_is_throttled_by_its_64_bit_prefix() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock, 2, 60, 4);
    for _ in 0..2 {
        t.record_failure(ip("2001:db8:1:2:aaaa::1"), "x@a.test");
    }
    assert!(matches!(t.check(ip("2001:db8:1:2:bbbb::9"), "x@a.test"), Verdict::Blocked { .. }), "rotating the low 64 bits must not help");
    assert_eq!(t.check(ip("2001:db8:1:3::1"), "x@a.test"), Verdict::Allowed, "another /64 is another peer");
}

#[test]
fn a_flood_of_new_keys_cannot_evict_an_active_block() {
    let clock = Arc::new(ManualClock::default());
    let t = throttle(clock.clone(), 3, 3600, 4);
    let victim = ip("192.0.2.1");
    for _ in 0..3 {
        t.record_failure(victim, "victim@a.test");
    }
    assert!(matches!(t.check(victim, "victim@a.test"), Verdict::Blocked { .. }));
    clock.advance_secs(1);
    for i in 0..150_000u32 {
        let attacker = IpAddr::from([10, (i >> 16) as u8, (i >> 8) as u8, i as u8]);
        t.record_failure(attacker, "x@a.test");
    }
    assert!(matches!(t.check(victim, "victim@a.test"), Verdict::Blocked { .. }), "the block survived the flood");
}
