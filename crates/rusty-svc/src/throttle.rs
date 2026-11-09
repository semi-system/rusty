//! `RateLimiter`: the rate-limit seam, with the in-memory authentication throttle as its
//! implementation. Keyed by (peer IP, identity); a store- or Valkey-backed limiter with per-tenant
//! keys is another implementation. The clock is injected so tests need no sleeping.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rusty_core::config::LimitsConfig;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Monotonic milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The process clock (`Instant`).
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock { start: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Blocked { retry_after_secs: u64 },
}

/// Proof of one running authentication attempt; dropping it frees the slot.
#[derive(Debug)]
pub struct AttemptPermit(#[allow(dead_code)] OwnedSemaphorePermit);

pub trait RateLimiter: Send + Sync {
    /// May this peer try to authenticate as `identity` now?
    fn check(&self, ip: IpAddr, identity: &str) -> Verdict;
    fn record_failure(&self, ip: IpAddr, identity: &str);
    fn record_success(&self, ip: IpAddr, identity: &str);
    /// Take a slot in the process-wide cap on concurrent authentication attempts (password
    /// hashing is expensive), or `None` when all slots are busy.
    fn try_begin_attempt(&self) -> Option<AttemptPermit>;
}

/// Most (peer, identity) pairs remembered at once.
const MAX_TRACKED: usize = 100_000;
/// Identities longer than this are cut for the key (an address is at most 254 bytes).
const MAX_IDENTITY_BYTES: usize = 320;

struct Entry {
    failures: u32,
    window_start_ms: u64,
}

pub struct AuthThrottle {
    max_failures: u32,
    window_ms: u64,
    clock: Arc<dyn Clock>,
    slots: Arc<Semaphore>,
    map: Mutex<HashMap<(IpAddr, String), Entry>>,
}

impl AuthThrottle {
    pub fn new(max_failures: u32, window_secs: u64, concurrent: u32, clock: Arc<dyn Clock>) -> AuthThrottle {
        AuthThrottle {
            max_failures,
            window_ms: window_secs.saturating_mul(1000),
            clock,
            slots: Arc::new(Semaphore::new(concurrent as usize)),
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_limits(limits: &LimitsConfig) -> AuthThrottle {
        Self::new(
            limits.auth_failures_max,
            limits.auth_window_secs,
            limits.auth_concurrent_max,
            Arc::new(SystemClock::new()),
        )
    }

    /// Number of pairs currently remembered (diagnostics and the memory-bound test).
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(IpAddr, String), Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn key(ip: IpAddr, identity: &str) -> (IpAddr, String) {
        // The directory's own normalisation (trim, case, trailing dot, NFC, IDNA), so every spelling
        // of one address is one key; anything that is not an address is kept as trimmed lower case.
        let mut id = match rusty_directory::parse_address(identity) {
            Ok((local, domain)) => format!("{local}@{domain}"),
            Err(_) => identity.trim().to_ascii_lowercase(),
        };
        if id.len() > MAX_IDENTITY_BYTES {
            let mut cut = MAX_IDENTITY_BYTES;
            while !id.is_char_boundary(cut) {
                cut -= 1;
            }
            id.truncate(cut);
        }
        // An IPv6 peer is its /64: the low 64 bits rotate freely.
        let ip = match ip {
            IpAddr::V6(v6) => {
                let mut o = v6.octets();
                o.iter_mut().skip(8).for_each(|b| *b = 0);
                IpAddr::V6(o.into())
            }
            v4 => v4,
        };
        (ip, id)
    }
}

impl RateLimiter for AuthThrottle {
    fn check(&self, ip: IpAddr, identity: &str) -> Verdict {
        let now = self.clock.now_ms();
        let m = self.lock();
        match m.get(&Self::key(ip, identity)) {
            Some(e) if e.failures >= self.max_failures && now.saturating_sub(e.window_start_ms) < self.window_ms => {
                let left_ms = self.window_ms - now.saturating_sub(e.window_start_ms);
                Verdict::Blocked { retry_after_secs: left_ms.div_ceil(1000).max(1) }
            }
            _ => Verdict::Allowed,
        }
    }

    fn record_failure(&self, ip: IpAddr, identity: &str) {
        let now = self.clock.now_ms();
        let key = Self::key(ip, identity);
        let mut m = self.lock();
        if !m.contains_key(&key) && m.len() >= MAX_TRACKED {
            // Make room: drop expired windows; if that frees too little, drop a tenth at random
            // (hash order), so a flood of identities cannot grow memory and the cost stays
            // amortised O(1) per insert.
            let window = self.window_ms;
            m.retain(|_, e| now.saturating_sub(e.window_start_ms) < window);
            if m.len() > MAX_TRACKED - MAX_TRACKED / 10 {
                // Evict the OLDEST entries that are not blocked: a flood of fresh keys must not erase
                // the state of a peer that is currently locked out.
                let max = self.max_failures;
                let mut evictable: Vec<_> = m.iter().filter(|(_, e)| e.failures < max).map(|(k, e)| (e.window_start_ms, k.clone())).collect();
                evictable.sort_unstable_by_key(|(t, _)| *t);
                for (_, k) in evictable.into_iter().take(MAX_TRACKED / 10) {
                    m.remove(&k);
                }
                if m.len() >= MAX_TRACKED {
                    return; // everything left is an active block: do not track one more
                }
            }
        }
        let e = m.entry(key).or_insert(Entry { failures: 0, window_start_ms: now });
        if now.saturating_sub(e.window_start_ms) >= self.window_ms {
            e.failures = 0;
            e.window_start_ms = now;
        }
        e.failures = e.failures.saturating_add(1);
    }

    fn record_success(&self, ip: IpAddr, identity: &str) {
        self.lock().remove(&Self::key(ip, identity));
    }

    fn try_begin_attempt(&self) -> Option<AttemptPermit> {
        self.slots.clone().try_acquire_owned().ok().map(AttemptPermit)
    }
}

/// The throttle keys of one authentication attempt (review U4 C1): the RESOLVED account (so the
/// alias, the primary address and every spelling share one counter) plus the canonical text as a
/// fallback counter that also bounds guessing at unknown identities. Check all of them, record
/// failures and successes on all of them.
pub async fn throttle_keys(directory: &rusty_directory::Directory, identity: &str) -> Vec<String> {
    let canonical = match rusty_directory::parse_address(identity) {
        Ok((local, domain)) => format!("{local}@{domain}"),
        Err(_) => return vec![identity.trim().to_ascii_lowercase()],
    };
    let mut keys = Vec::with_capacity(2);
    if let Ok(rusty_directory::Resolved::Account(id)) = directory.resolve(&canonical).await {
        keys.push(format!("account:{}", id.0));
    }
    keys.push(canonical);
    keys
}
