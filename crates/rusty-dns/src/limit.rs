//! The abuse limits of the name server (docs/design-authoritative-dns.md, section 3): a UDP answer is bigger than the question that asked for it, and the source address of a UDP question can be forged, so a name server is
//! a tool for flooding a third party unless it limits what it says to one source. This is the response rate limit: a token bucket per source **prefix** (a /24 of IPv4 or a /56 of IPv6, so that one machine cannot
//! use a whole block of addresses to get around it). A question over the limit is not answered, except that every `slip`th one gets an empty truncated answer: a forged source gets a packet too small to hurt, and a real
//! client behind that prefix retries over TCP (which cannot be forged) and is served.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What to do with a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Answer,
    /// Over the limit: send nothing.
    Drop,
    /// Over the limit: send an empty answer with `TC` set.
    Slip,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Answers a second one prefix may have.
    pub per_second: f64,
    /// How many it may have at once (a burst).
    pub burst: f64,
    /// One in this many questions over the limit gets a truncated answer (0: none ever).
    pub slip: u32,
    /// The most prefixes remembered; when full, the idlest ones go first.
    pub max_prefixes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { per_second: 20.0, burst: 40.0, slip: 2, max_prefixes: 100_000 }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
    over: u32,
}

/// The block an address belongs to for limiting.
pub fn prefix(ip: IpAddr) -> [u8; 8] {
    let mut k = [0u8; 8];
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            k[0] = 4;
            k[1..4].copy_from_slice(&o[..3]);
        }
        IpAddr::V6(a) => {
            let o = a.octets();
            k[0] = 6;
            k[1..8].copy_from_slice(&o[..7]);
        }
    }
    k
}

pub struct RateLimiter {
    limits: Limits,
    buckets: Mutex<HashMap<[u8; 8], Bucket>>,
}

impl RateLimiter {
    pub fn new(limits: Limits) -> RateLimiter {
        RateLimiter { limits, buckets: Mutex::new(HashMap::new()) }
    }

    pub fn check(&self, ip: IpAddr) -> Verdict {
        self.check_at(ip, Instant::now())
    }

    pub fn check_at(&self, ip: IpAddr, now: Instant) -> Verdict {
        let key = prefix(ip);
        let Ok(mut map) = self.buckets.lock() else { return Verdict::Answer };
        if map.len() >= self.limits.max_prefixes && !map.contains_key(&key) {
            // Full: forget the idlest tenth (a table that cannot grow is the point; the cost is that an old prefix starts again with a full bucket).
            let mut by_age: Vec<([u8; 8], Instant)> = map.iter().map(|(k, b)| (*k, b.last)).collect();
            by_age.sort_by_key(|(_, t)| *t);
            for (k, _) in by_age.into_iter().take(self.limits.max_prefixes / 10 + 1) {
                map.remove(&k);
            }
        }
        let b = map.entry(key).or_insert(Bucket { tokens: self.limits.burst, last: now, over: 0 });
        let gained = now.saturating_duration_since(b.last).as_secs_f64() * self.limits.per_second;
        b.tokens = (b.tokens + gained).min(self.limits.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            b.over = 0;
            return Verdict::Answer;
        }
        b.over = b.over.saturating_add(1);
        if self.limits.slip > 0 && b.over % self.limits.slip == 0 {
            Verdict::Slip
        } else {
            Verdict::Drop
        }
    }

    /// Forget the prefixes idle for `idle` (called now and then, so the table does not hold what is long gone).
    pub fn sweep(&self, idle: Duration) {
        let now = Instant::now();
        if let Ok(mut map) = self.buckets.lock() {
            map.retain(|_, b| now.saturating_duration_since(b.last) < idle);
        }
    }

    pub fn tracked(&self) -> usize {
        self.buckets.lock().map_or(0, |m| m.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_burst_is_answered_then_the_limit_drops_and_slips_and_time_gives_tokens_back() {
        let l = RateLimiter::new(Limits { per_second: 10.0, burst: 5.0, slip: 2, max_prefixes: 100 });
        let t0 = Instant::now();
        let first: Vec<Verdict> = (0..5).map(|_| l.check_at(ip("198.51.100.7"), t0)).collect();
        assert!(first.iter().all(|v| *v == Verdict::Answer));
        let over: Vec<Verdict> = (0..4).map(|_| l.check_at(ip("198.51.100.7"), t0)).collect();
        assert_eq!(over, [Verdict::Drop, Verdict::Slip, Verdict::Drop, Verdict::Slip]);
        // A tenth of a second later one token is back.
        assert_eq!(l.check_at(ip("198.51.100.7"), t0 + Duration::from_millis(100)), Verdict::Answer);
        assert_ne!(l.check_at(ip("198.51.100.7"), t0 + Duration::from_millis(100)), Verdict::Answer);
    }

    #[test]
    fn a_whole_prefix_shares_one_bucket_and_another_prefix_has_its_own() {
        let l = RateLimiter::new(Limits { per_second: 1.0, burst: 2.0, slip: 0, max_prefixes: 100 });
        let t = Instant::now();
        assert_eq!(l.check_at(ip("203.0.113.1"), t), Verdict::Answer);
        assert_eq!(l.check_at(ip("203.0.113.200"), t), Verdict::Answer, "the same /24");
        assert_eq!(l.check_at(ip("203.0.113.99"), t), Verdict::Drop, "no slip when slip is 0");
        assert_eq!(l.check_at(ip("203.0.114.1"), t), Verdict::Answer, "another /24");
        assert_eq!(l.check_at(ip("2001:db8:0:0:1::1"), t), Verdict::Answer);
        assert_eq!(l.check_at(ip("2001:db8:0:0:2::9"), t), Verdict::Answer, "the same /56");
        assert_eq!(l.check_at(ip("2001:db8:0:0:3::9"), t), Verdict::Drop);
    }

    #[test]
    fn the_table_cannot_grow_past_its_bound_and_idle_prefixes_are_swept() {
        let l = RateLimiter::new(Limits { per_second: 1.0, burst: 1.0, slip: 0, max_prefixes: 50 });
        let t = Instant::now();
        for i in 0..500u32 {
            let ip = IpAddr::from([10, (i >> 8) as u8, (i & 0xff) as u8, 1]);
            l.check_at(ip, t + Duration::from_millis(u64::from(i)));
        }
        assert!(l.tracked() <= 50, "{}", l.tracked());
        l.sweep(Duration::ZERO);
        assert_eq!(l.tracked(), 0);
    }
}
