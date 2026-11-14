//! `rusty-valkey`: the shared key-value store (Valkey, or Redis-compatible) behind one small trait.
//!
//! It serves three consumers: rate limits and brute-force counters, login sessions, and a read cache. None of
//! them may depend on it for correctness: every consumer keeps working, with its own in-process state, when the
//! store is not configured or not reachable (`Error::Busy` from any call means "unavailable, fall back").
//!
//! * [`Kv`] is the Rusty-owned interface: a sliding window, a counter, and expiring values.
//! * [`MemoryKv`] implements it in process (a manual or system clock): the fallback, and what the tests of the
//!   consumers use.
//! * [`ValkeyKv`] implements it over Valkey. The client library lives only in `src/adapter`
//!   (`scripts/wrapped-libs.txt`), so a consumer never sees its types.
//!
//! Keys are short printable ASCII and values are bounded (see [`MAX_KEY_BYTES`], [`MAX_VALUE_BYTES`]); a caller
//! that breaks that gets `Error::Invalid`, never a truncated or escaped key.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod adapter;
mod clock;
mod memory;

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};

pub use adapter::{TlsOptions, ValkeyKv, ValkeyOptions};
pub use clock::{Clock, ManualClock, SystemClock};
pub use memory::MemoryKv;

/// Longest key a caller may use (the configured prefix is added on top).
pub const MAX_KEY_BYTES: usize = 200;
/// Largest value `set` accepts.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// The result of [`Kv::window_try_record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowOutcome {
    /// The event was counted (it was under the limit).
    pub allowed: bool,
    /// Events inside the window after the call.
    pub count: u32,
}

#[async_trait]
pub trait Kv: Send + Sync {
    /// The store answers.
    async fn ping(&self) -> Result<()>;

    /// Sliding window, atomically: forget events older than `window`, and if fewer than `limit` remain count
    /// one more. Parallel callers cannot all pass a full window. The clock is the store's own, so replicas
    /// with skewed clocks agree.
    async fn window_try_record(&self, key: &str, window: Duration, limit: u32) -> Result<WindowOutcome>;

    /// Events inside the window now (nothing is counted).
    async fn window_count(&self, key: &str, window: Duration) -> Result<u32>;

    /// Add one to a counter and return it. The counter lives `ttl` from its FIRST increment (later increments
    /// do not extend it), so "N failures per period" is a fixed period.
    async fn counter_incr(&self, key: &str, ttl: Duration) -> Result<u64>;

    /// The counter, or 0 when it does not exist.
    async fn counter_get(&self, key: &str) -> Result<u64>;

    /// Store `value` for `ttl`, replacing what was there.
    async fn set(&self, key: &str, value: &[u8], ttl: Duration) -> Result<()>;

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Give a live key `ttl` more from now; `false` when it does not exist (nothing is created).
    async fn touch(&self, key: &str, ttl: Duration) -> Result<bool>;

    /// Forget a key of any kind; a missing key is fine.
    async fn delete(&self, key: &str) -> Result<()>;
}

/// A key is 1 to [`MAX_KEY_BYTES`] printable ASCII characters without a space.
pub(crate) fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty() && key.len() <= MAX_KEY_BYTES && key.bytes().all(|b| (0x21..=0x7e).contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid("key-value key must be 1 to 200 printable ASCII characters".into()))
    }
}

pub(crate) fn check_ttl(ttl: Duration) -> Result<()> {
    if ttl.is_zero() || ttl > Duration::from_secs(366 * 24 * 3600) {
        return Err(Error::Invalid("key-value ttl must be between 1 ms and a year".into()));
    }
    Ok(())
}

pub(crate) fn check_value(value: &[u8]) -> Result<()> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(Error::Invalid("key-value value is too large".into()));
    }
    Ok(())
}

pub(crate) fn check_window(window: Duration, limit: u32) -> Result<()> {
    check_ttl(window)?;
    if limit == 0 {
        return Err(Error::Invalid("key-value window limit must be at least 1".into()));
    }
    Ok(())
}

/// The one error a consumer sees when the store cannot be used. No host, no URL, no driver text: those can
/// name the server and are only logged at debug level by the adapter.
pub(crate) fn unavailable() -> Error {
    Error::Busy("key-value store unavailable".into())
}
