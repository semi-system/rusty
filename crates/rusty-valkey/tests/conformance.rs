//! One suite, two implementations of `Kv`: the in-memory one (manual clock, no sleeping) and the Valkey one
//! (real server, real time). Both must behave the same, because the in-memory one is what a consumer falls
//! back to and what the unit tests of the consumers use.
//!
//! The Valkey tests need `RUSTY_TEST_VALKEY_URL` (scripts/kv-up.sh starts one). Without it they are skipped,
//! unless `RUSTY_REQUIRE_VALKEY=1` (CI sets it): then a missing server fails the test.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::Error;
use rusty_valkey::{Kv, ManualClock, MemoryKv, MAX_KEY_BYTES, MAX_VALUE_BYTES};

mod common;
use common::valkey_kv;

/// What a test needs besides the store: a way to let time pass and a key namespace of its own (the Valkey
/// server is shared by tests running in parallel).
struct Harness {
    kv: Arc<dyn Kv>,
    clock: Option<Arc<ManualClock>>,
    ns: String,
}

impl Harness {
    fn key(&self, name: &str) -> String {
        format!("{}:{name}", self.ns)
    }

    async fn advance(&self, d: Duration) {
        match &self.clock {
            Some(c) => c.advance(d),
            None => tokio::time::sleep(d).await,
        }
    }
}

fn memory() -> Harness {
    let clock = Arc::new(ManualClock::new());
    Harness { kv: Arc::new(MemoryKv::with_clock(clock.clone())), clock: Some(clock), ns: "m".into() }
}

async fn valkey() -> Option<Harness> {
    let kv = valkey_kv("conformance").await?;
    let ns = format!("c{:x}", std::process::id() ^ rand_token());
    Some(Harness { kv: Arc::new(kv), clock: None, ns })
}

fn rand_token() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(1);
    N.fetch_add(1, Ordering::Relaxed).wrapping_mul(2_654_435_761)
}

// ---------------------------------------------------------------------------------------------------------

async fn window_allows_up_to_the_limit_then_refuses(h: &Harness) {
    let k = h.key("w1");
    let window = Duration::from_millis(1000);
    for i in 1..=3u32 {
        let o = h.kv.window_try_record(&k, window, 3).await.unwrap();
        assert!(o.allowed, "call {i}");
        assert_eq!(o.count, i);
    }
    let o = h.kv.window_try_record(&k, window, 3).await.unwrap();
    assert!(!o.allowed, "the fourth call is over the limit");
    assert_eq!(o.count, 3, "a refused call counts nothing");
    assert_eq!(h.kv.window_count(&k, window).await.unwrap(), 3);
}

async fn window_slides_instead_of_resetting(h: &Harness) {
    let k = h.key("w2");
    let window = Duration::from_millis(1000);
    assert!(h.kv.window_try_record(&k, window, 2).await.unwrap().allowed); // t = 0
    h.advance(Duration::from_millis(550)).await;
    assert!(h.kv.window_try_record(&k, window, 2).await.unwrap().allowed); // t = 550
    assert!(!h.kv.window_try_record(&k, window, 2).await.unwrap().allowed);
    h.advance(Duration::from_millis(550)).await; // t = 1100: the first entry is out of the window, the second is not
    assert_eq!(h.kv.window_count(&k, window).await.unwrap(), 1);
    assert!(h.kv.window_try_record(&k, window, 2).await.unwrap().allowed);
    assert!(!h.kv.window_try_record(&k, window, 2).await.unwrap().allowed);
}

async fn windows_of_different_keys_are_independent(h: &Harness) {
    let (a, b) = (h.key("wa"), h.key("wb"));
    let window = Duration::from_secs(5);
    assert!(h.kv.window_try_record(&a, window, 1).await.unwrap().allowed);
    assert!(!h.kv.window_try_record(&a, window, 1).await.unwrap().allowed);
    assert!(h.kv.window_try_record(&b, window, 1).await.unwrap().allowed);
}

async fn parallel_callers_cannot_pass_a_full_window(h: &Harness) {
    let k = h.key("w3");
    let mut tasks = Vec::new();
    for _ in 0..60 {
        let (kv, k) = (h.kv.clone(), k.clone());
        tasks.push(tokio::spawn(async move { kv.window_try_record(&k, Duration::from_secs(30), 10).await.unwrap().allowed }));
    }
    let mut allowed = 0;
    for t in tasks {
        if t.await.unwrap() {
            allowed += 1;
        }
    }
    assert_eq!(allowed, 10, "exactly the limit passes however many race for it");
}

async fn counter_counts_and_expires_without_extending_its_ttl(h: &Harness) {
    let k = h.key("c1");
    let ttl = Duration::from_millis(1500);
    assert_eq!(h.kv.counter_get(&k).await.unwrap(), 0, "a missing counter is 0");
    assert_eq!(h.kv.counter_incr(&k, ttl).await.unwrap(), 1);
    h.advance(Duration::from_millis(800)).await;
    assert_eq!(h.kv.counter_incr(&k, ttl).await.unwrap(), 2, "the second increment must not extend the life");
    assert_eq!(h.kv.counter_get(&k).await.unwrap(), 2);
    h.advance(Duration::from_millis(800)).await; // 1600 ms after the first increment
    assert_eq!(h.kv.counter_get(&k).await.unwrap(), 0, "the counter lives ttl from its FIRST increment");
    assert_eq!(h.kv.counter_incr(&k, ttl).await.unwrap(), 1, "and starts over");
}

async fn delete_forgets_everything_under_a_key(h: &Harness) {
    let (c, w, v) = (h.key("d-counter"), h.key("d-window"), h.key("d-value"));
    let ttl = Duration::from_secs(30);
    h.kv.counter_incr(&c, ttl).await.unwrap();
    h.kv.window_try_record(&w, ttl, 5).await.unwrap();
    h.kv.set(&v, b"x", ttl).await.unwrap();
    for k in [&c, &w, &v] {
        h.kv.delete(k).await.unwrap();
    }
    assert_eq!(h.kv.counter_get(&c).await.unwrap(), 0);
    assert_eq!(h.kv.window_count(&w, ttl).await.unwrap(), 0);
    assert_eq!(h.kv.get(&v).await.unwrap(), None);
    h.kv.delete(&v).await.unwrap(); // deleting what is not there is fine
}

async fn values_round_trip_every_byte_and_expire(h: &Harness) {
    let k = h.key("v1");
    let all: Vec<u8> = (0..=255u8).collect();
    h.kv.set(&k, &all, Duration::from_millis(1200)).await.unwrap();
    assert_eq!(h.kv.get(&k).await.unwrap().as_deref(), Some(all.as_slice()));
    h.advance(Duration::from_millis(1400)).await;
    assert_eq!(h.kv.get(&k).await.unwrap(), None, "a value does not outlive its ttl");
    assert_eq!(h.kv.get(&h.key("never-set")).await.unwrap(), None);
}

async fn touch_extends_a_live_value_only(h: &Harness) {
    let k = h.key("v2");
    h.kv.set(&k, b"session", Duration::from_millis(1200)).await.unwrap();
    h.advance(Duration::from_millis(700)).await;
    assert!(h.kv.touch(&k, Duration::from_millis(1200)).await.unwrap());
    h.advance(Duration::from_millis(700)).await; // 1400 ms after the set, 700 after the touch
    assert_eq!(h.kv.get(&k).await.unwrap().as_deref(), Some(&b"session"[..]));
    h.advance(Duration::from_millis(700)).await;
    assert_eq!(h.kv.get(&k).await.unwrap(), None);
    assert!(!h.kv.touch(&k, Duration::from_secs(5)).await.unwrap(), "touching a missing key creates nothing");
    assert_eq!(h.kv.get(&k).await.unwrap(), None);
}

async fn set_replaces_and_restarts_the_ttl(h: &Harness) {
    let k = h.key("v3");
    h.kv.set(&k, b"one", Duration::from_millis(1000)).await.unwrap();
    h.advance(Duration::from_millis(600)).await;
    h.kv.set(&k, b"two", Duration::from_millis(1000)).await.unwrap();
    h.advance(Duration::from_millis(600)).await;
    assert_eq!(h.kv.get(&k).await.unwrap().as_deref(), Some(&b"two"[..]));
}

async fn keys_and_values_are_bounded(h: &Harness) {
    let ttl = Duration::from_secs(5);
    let too_long = "k".repeat(MAX_KEY_BYTES + 1);
    for bad in ["", " ", "a b", "tab\t", "nul\0", "new\nline", "é", too_long.as_str()] {
        assert!(matches!(h.kv.counter_incr(bad, ttl).await, Err(Error::Invalid(_))), "{bad:?}");
        assert!(matches!(h.kv.get(bad).await, Err(Error::Invalid(_))), "{bad:?}");
        assert!(matches!(h.kv.window_try_record(bad, ttl, 1).await, Err(Error::Invalid(_))), "{bad:?}");
    }
    let k = h.key("big");
    let huge = vec![0u8; MAX_VALUE_BYTES + 1];
    assert!(matches!(h.kv.set(&k, &huge, ttl).await, Err(Error::Invalid(_))));
    h.kv.set(&k, &huge[..MAX_VALUE_BYTES], ttl).await.unwrap();
    // A zero life or a zero limit is a caller's bug, not an instruction to store forever.
    assert!(matches!(h.kv.set(&k, b"x", Duration::ZERO).await, Err(Error::Invalid(_))));
    assert!(matches!(h.kv.counter_incr(&k, Duration::ZERO).await, Err(Error::Invalid(_))));
    assert!(matches!(h.kv.window_try_record(&k, Duration::ZERO, 1).await, Err(Error::Invalid(_))));
    assert!(matches!(h.kv.window_try_record(&k, ttl, 0).await, Err(Error::Invalid(_))));
}

async fn ping_works(h: &Harness) {
    h.kv.ping().await.unwrap();
}

macro_rules! suite {
    ($($name:ident),+ $(,)?) => {
        mod in_memory {
            use super::*;
            $( #[tokio::test] async fn $name() { super::$name(&memory()).await; } )+
        }
        mod against_valkey {
            use super::*;
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $name() {
                let Some(h) = valkey().await else { return };
                super::$name(&h).await;
            } )+
        }
    };
}

suite!(
    window_allows_up_to_the_limit_then_refuses,
    window_slides_instead_of_resetting,
    windows_of_different_keys_are_independent,
    parallel_callers_cannot_pass_a_full_window,
    counter_counts_and_expires_without_extending_its_ttl,
    delete_forgets_everything_under_a_key,
    values_round_trip_every_byte_and_expire,
    touch_extends_a_live_value_only,
    set_replaces_and_restarts_the_ttl,
    keys_and_values_are_bounded,
    ping_works,
);
