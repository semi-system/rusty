//! The clock of [`crate::MemoryKv`]: monotonic milliseconds, injectable so tests need no sleeping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync {
    /// Milliseconds since some fixed origin; never goes backwards.
    fn now_ms(&self) -> u64;
}

/// The process clock.
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

/// A clock that only moves when told to (tests).
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new() -> ManualClock {
        ManualClock(AtomicU64::new(1_000))
    }

    pub fn advance(&self, d: Duration) {
        self.0.fetch_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
