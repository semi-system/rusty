//! `ValkeyThrottle`: the authentication throttle shared by every instance of a service (and by the services that authenticate: submission and IMAP count against the same
//! pair), kept in the key-value store of `[valkey]`. Without it each replica has its own counters, so N replicas allow N times the failures.
//!
//! * A failure counts in a fixed window that starts at the first failure of a (peer, identity) pair (`counter_incr`, the store's own clock): `auth_failures_max` failures per
//!   `auth_window_secs`. The keys are the ones of the in-memory throttle (the IPv6 peer as its /64, the identity normalised) with the identity hashed, so no address of a user is
//!   written into the store's key names.
//! * The in-memory throttle runs alongside as the floor: a store that cannot be reached must not switch the throttle off, so every call is also counted here and an unreachable
//!   store is logged and ignored (the process's own counters still limit guessing).
//! * A block answers `Blocked` with the whole window as the retry time: the store does not say how much of the window is left, so the answer is never too short.
//! * The cap on concurrent attempts (password hashing is CPU) stays per process.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::config::LimitsConfig;
use rusty_core::BlobHash;
use rusty_valkey::Kv;

use crate::throttle::{AttemptPermit, AuthThrottle, Clock, RateLimiter, SystemClock, Verdict};

pub struct ValkeyThrottle {
    kv: Arc<dyn Kv>,
    local: AuthThrottle,
    max_failures: u32,
    window: Duration,
}

impl ValkeyThrottle {
    pub fn new(kv: Arc<dyn Kv>, max_failures: u32, window_secs: u64, concurrent: u32, clock: Arc<dyn Clock>) -> ValkeyThrottle {
        ValkeyThrottle { kv, local: AuthThrottle::new(max_failures, window_secs, concurrent, clock), max_failures, window: Duration::from_secs(window_secs.max(1)) }
    }

    pub fn from_limits(kv: Arc<dyn Kv>, limits: &LimitsConfig) -> ValkeyThrottle {
        ValkeyThrottle::new(kv, limits.auth_failures_max, limits.auth_window_secs, limits.auth_concurrent_max, Arc::new(SystemClock::new()))
    }

    fn key(ip: IpAddr, identity: &str) -> String {
        let (ip, id) = AuthThrottle::key(ip, identity);
        let digest = BlobHash::of(id.as_bytes());
        format!("thr:{ip}:{}", digest.as_str().get(..32).unwrap_or_default())
    }
}

#[async_trait]
impl RateLimiter for ValkeyThrottle {
    async fn check(&self, ip: IpAddr, identity: &str) -> Verdict {
        let local = self.local.check(ip, identity).await;
        if matches!(local, Verdict::Blocked { .. }) {
            return local;
        }
        match self.kv.counter_get(&Self::key(ip, identity)).await {
            Ok(n) if n >= u64::from(self.max_failures) => Verdict::Blocked { retry_after_secs: self.window.as_secs().max(1) },
            Ok(_) => Verdict::Allowed,
            Err(e) => {
                tracing::warn!(error = %e, "the shared authentication throttle is unreachable; only this process's counters apply");
                Verdict::Allowed
            }
        }
    }

    async fn record_failure(&self, ip: IpAddr, identity: &str) {
        self.local.record_failure(ip, identity).await;
        if let Err(e) = self.kv.counter_incr(&Self::key(ip, identity), self.window).await {
            tracing::warn!(error = %e, "a failed sign-in could not be counted in the shared throttle");
        }
    }

    async fn record_success(&self, ip: IpAddr, identity: &str) {
        self.local.record_success(ip, identity).await;
        if let Err(e) = self.kv.delete(&Self::key(ip, identity)).await {
            tracing::warn!(error = %e, "the shared throttle could not be cleared after a sign-in");
        }
    }

    fn try_begin_attempt(&self) -> Option<AttemptPermit> {
        self.local.try_begin_attempt()
    }
}
