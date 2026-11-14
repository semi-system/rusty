//! [`MemoryKv`]: the in-process implementation of [`Kv`]. The fallback of every consumer and the
//! store of their unit tests; it must behave like the Valkey one (`tests/conformance.rs` runs the same suite).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};

use crate::{
    check_key, check_ttl, check_value, check_window, clock::SystemClock, unavailable, Clock, Kv, WindowOutcome,
};

/// Most keys held at once. Expired ones are dropped first; when still full, a NEW key is refused
/// (`Busy`, the consumer then decides) rather than growing without bound.
const MAX_ENTRIES: usize = 100_000;

enum Entry {
    /// Timestamps (ms) of the events inside the window.
    Window(VecDeque<u64>),
    Counter { n: u64, expires_ms: u64 },
    Value { bytes: Vec<u8>, expires_ms: u64 },
}

pub struct MemoryKv {
    clock: Arc<dyn Clock>,
    map: Mutex<HashMap<String, Entry>>,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn wrong_kind() -> Error {
    Error::Invalid("key-value key holds another kind of value".into())
}

impl MemoryKv {
    pub fn new() -> MemoryKv {
        MemoryKv::with_clock(Arc::new(SystemClock::new()))
    }

    pub fn with_clock(clock: Arc<dyn Clock>) -> MemoryKv {
        MemoryKv { clock, map: Mutex::new(HashMap::new()) }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop what is over, then make room for one more key or refuse.
    fn room(map: &mut HashMap<String, Entry>, key: &str, now: u64) -> Result<()> {
        if map.contains_key(key) || map.len() < MAX_ENTRIES {
            return Ok(());
        }
        map.retain(|_, e| match e {
            Entry::Window(w) => w.back().is_some(),
            Entry::Counter { expires_ms, .. } | Entry::Value { expires_ms, .. } => *expires_ms > now,
        });
        if map.len() < MAX_ENTRIES {
            Ok(())
        } else {
            Err(unavailable())
        }
    }

    /// The live window of `key` (events older than `window` removed), creating it when `create`.
    fn window<'a>(
        map: &'a mut HashMap<String, Entry>,
        key: &str,
        now: u64,
        window_ms: u64,
        create: bool,
    ) -> Result<Option<&'a mut VecDeque<u64>>> {
        if create {
            Self::room(map, key, now)?;
        }
        // A counter or a value that has expired is gone, so the key is free for another kind.
        if let Some(Entry::Counter { expires_ms, .. } | Entry::Value { expires_ms, .. }) = map.get(key) {
            if *expires_ms <= now {
                map.remove(key);
            }
        }
        let entry = if create {
            Some(map.entry(key.to_owned()).or_insert_with(|| Entry::Window(VecDeque::new())))
        } else {
            map.get_mut(key)
        };
        match entry {
            None => Ok(None),
            Some(Entry::Window(w)) => {
                // Valkey's ZREMRANGEBYSCORE -inf (now - window) is inclusive: an event exactly one window old is out.
                while w.front().is_some_and(|&t| t.saturating_add(window_ms) <= now) {
                    w.pop_front();
                }
                Ok(Some(w))
            }
            Some(_) => Err(wrong_kind()),
        }
    }
}

impl Default for MemoryKv {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Kv for MemoryKv {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn window_try_record(&self, key: &str, window: Duration, limit: u32) -> Result<WindowOutcome> {
        check_key(key)?;
        check_window(window, limit)?;
        let now = self.clock.now_ms();
        let mut map = self.lock();
        let Some(w) = Self::window(&mut map, key, now, millis(window), true)? else {
            return Err(unavailable());
        };
        let count = u32::try_from(w.len()).unwrap_or(u32::MAX);
        if count < limit {
            w.push_back(now);
            Ok(WindowOutcome { allowed: true, count: count + 1 })
        } else {
            Ok(WindowOutcome { allowed: false, count })
        }
    }

    async fn window_count(&self, key: &str, window: Duration) -> Result<u32> {
        check_key(key)?;
        check_ttl(window)?;
        let now = self.clock.now_ms();
        let mut map = self.lock();
        Ok(Self::window(&mut map, key, now, millis(window), false)?
            .map_or(0, |w| u32::try_from(w.len()).unwrap_or(u32::MAX)))
    }

    async fn counter_incr(&self, key: &str, ttl: Duration) -> Result<u64> {
        check_key(key)?;
        check_ttl(ttl)?;
        let now = self.clock.now_ms();
        let mut map = self.lock();
        Self::room(&mut map, key, now)?;
        if let Some(Entry::Counter { expires_ms, .. }) = map.get(key) {
            if *expires_ms <= now {
                map.remove(key);
            }
        }
        match map.entry(key.to_owned()).or_insert(Entry::Counter { n: 0, expires_ms: now.saturating_add(millis(ttl)) }) {
            Entry::Counter { n, .. } => {
                *n = n.saturating_add(1);
                Ok(*n)
            }
            _ => Err(wrong_kind()),
        }
    }

    async fn counter_get(&self, key: &str) -> Result<u64> {
        check_key(key)?;
        let now = self.clock.now_ms();
        match self.lock().get(key) {
            None => Ok(0),
            Some(Entry::Counter { n, expires_ms }) => Ok(if *expires_ms > now { *n } else { 0 }),
            Some(_) => Err(wrong_kind()),
        }
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Duration) -> Result<()> {
        check_key(key)?;
        check_ttl(ttl)?;
        check_value(value)?;
        let now = self.clock.now_ms();
        let mut map = self.lock();
        Self::room(&mut map, key, now)?;
        map.insert(key.to_owned(), Entry::Value { bytes: value.to_vec(), expires_ms: now.saturating_add(millis(ttl)) });
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        let now = self.clock.now_ms();
        match self.lock().get(key) {
            None => Ok(None),
            Some(Entry::Value { bytes, expires_ms }) => Ok((*expires_ms > now).then(|| bytes.clone())),
            Some(_) => Err(wrong_kind()),
        }
    }

    async fn touch(&self, key: &str, ttl: Duration) -> Result<bool> {
        check_key(key)?;
        check_ttl(ttl)?;
        let now = self.clock.now_ms();
        let mut map = self.lock();
        match map.get_mut(key) {
            Some(Entry::Value { expires_ms, .. } | Entry::Counter { expires_ms, .. }) if *expires_ms > now => {
                *expires_ms = now.saturating_add(millis(ttl));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        self.lock().remove(key);
        Ok(())
    }
}
