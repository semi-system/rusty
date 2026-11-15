//! `PoolStore`: one blob store over several backends (docs/design-s3-per-tenant.md, section 12).
//!
//! The pool is itself a [`BlobStore`], so nothing above it (delivery, IMAP, GC) knows it is a pool. Which backend holds an object is recorded in `blob_locations` (the authority for reads);
//! the placement rule only chooses where a *new* copy goes. The three behaviours a tenant asks for are presets of four settings ([`PoolPolicy`]):
//!
//! | behaviour | copies | sync_copies | placement | read |
//! |---|---|---|---|---|
//! | JBOD | 1 | 1 | `weighted` | `located` |
//! | mirror | N | N | `weighted` or `ordered` | `round_robin` or `fastest` |
//! | failover | 2+ | 1 | `ordered` | `ordered` |
//!
//! * **Write**: the backends that already hold the object come first (a repeated put refreshes their modification time, which is what restarts garbage collection's grace window),
//!   then the others by the placement rule. `copies` writes start together; the put succeeds when `sync_copies` of them are confirmed and recorded in `blob_locations`; a copy that fails is
//!   replaced by the next candidate; the copies that are still running finish and record themselves. Fewer than `sync_copies` healthy writable backends is `Busy` (the sender retries).
//! * **Read**: the recorded holders that can be used, ordered by the read rule; the next one is tried on an error, a missing object or bytes that do not match the name (a bad copy is
//!   forgotten, so repair makes a new one). An object with no recorded location is looked for on every backend and, when found, recorded.
//! * **Health**: every backend has a circuit breaker (consecutive failures open it for a while); an open backend is left to last in reads and is not written to.
//! * **Delete** (garbage collection): the blob row is already gone; the object is deleted wherever it is. A backend that cannot be reached keeps its location as `pending_delete`
//!   and [`PoolStore::repair`] retries it, so garbage does not live forever on a backend that was down when it was collected.
//! * **Repair** ([`PoolStore::repair`]): outstanding deletions, objects with fewer than `copies` copies, objects on a draining backend.
//!
//! The backends are plain blob stores and are required to verify what they read; the pool verifies again, so a copy that is wrong is never returned.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{BlobHash, Error, Result};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::blob::{BlobEntry, BlobStore, GcLock, HealReport};
use crate::repo::{BackendId, BackendRow, BackendState, BlobPoolRepo, StoreId, MAX_BACKENDS};

/// How objects are spread over the backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The listed order, skipping backends that are not usable.
    Ordered,
    /// Rendezvous hashing over the weights: deterministic for a given set of backends and even over many objects.
    Weighted,
}

/// In which order the holders of an object are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRule {
    /// The recorded holders in the order of the backends (what JBOD, with one holder per object, needs).
    Located,
    Ordered,
    RoundRobin,
    /// The holder that answered fastest lately.
    Fastest,
}

/// The settings of a pool, from `blob_stores.config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPolicy {
    pub copies: u32,
    pub sync_copies: u32,
    pub placement: Placement,
    pub read: ReadRule,
}

impl Default for PoolPolicy {
    /// One copy on one backend chosen by weight: JBOD.
    fn default() -> PoolPolicy {
        PoolPolicy { copies: 1, sync_copies: 1, placement: Placement::Weighted, read: ReadRule::Located }
    }
}

impl PoolPolicy {
    /// Parse and validate `{"copies":2,"sync_copies":1,"placement":"ordered","read":"ordered"}`; every field is optional, an unknown field is refused.
    pub fn parse(config: &str) -> Result<PoolPolicy> {
        let v: serde_json::Value = serde_json::from_str(config).map_err(|_| Error::Invalid("the pool settings are not JSON".into()))?;
        let obj = v.as_object().ok_or_else(|| Error::Invalid("the pool settings are not an object".into()))?;
        let mut p = PoolPolicy::default();
        for (k, val) in obj {
            match k.as_str() {
                "copies" => p.copies = small(val)?,
                "sync_copies" => p.sync_copies = small(val)?,
                "placement" => {
                    p.placement = match val.as_str() {
                        Some("ordered") => Placement::Ordered,
                        Some("weighted") => Placement::Weighted,
                        _ => return Err(Error::Invalid("placement is ordered or weighted".into())),
                    }
                }
                "read" => {
                    p.read = match val.as_str() {
                        Some("located") => ReadRule::Located,
                        Some("ordered") => ReadRule::Ordered,
                        Some("round_robin") => ReadRule::RoundRobin,
                        Some("fastest") => ReadRule::Fastest,
                        _ => return Err(Error::Invalid("read is located, ordered, round_robin or fastest".into())),
                    }
                }
                _ => return Err(Error::Invalid("unknown pool setting".into())),
            }
        }
        if obj.contains_key("copies") && !obj.contains_key("sync_copies") {
            // "copies": 2 alone means a mirror: both copies before the message is acknowledged.
            p.sync_copies = p.copies;
        }
        p.check()?;
        Ok(p)
    }

    fn check(&self) -> Result<()> {
        let max = u32::try_from(MAX_BACKENDS).unwrap_or(8);
        if !(1..=max).contains(&self.copies) || self.sync_copies < 1 || self.sync_copies > self.copies {
            return Err(Error::Invalid("a pool needs 1 <= sync_copies <= copies <= 8".into()));
        }
        Ok(())
    }

    /// The settings as JSON text (what `parse` reads back).
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "copies": self.copies,
            "sync_copies": self.sync_copies,
            "placement": match self.placement { Placement::Ordered => "ordered", Placement::Weighted => "weighted" },
            "read": match self.read { ReadRule::Located => "located", ReadRule::Ordered => "ordered", ReadRule::RoundRobin => "round_robin", ReadRule::Fastest => "fastest" },
        })
        .to_string()
    }
}

fn small(v: &serde_json::Value) -> Result<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n <= 64).ok_or_else(|| Error::Invalid("a pool count is a small whole number".into()))
}

/// Consecutive failures that open a backend's breaker, and for how long it stays open.
#[derive(Debug, Clone, Copy)]
pub struct Breaker {
    pub threshold: u32,
    pub cooldown: Duration,
}

impl Default for Breaker {
    fn default() -> Breaker {
        Breaker { threshold: 5, cooldown: Duration::from_secs(30) }
    }
}

struct Health {
    breaker: Breaker,
    fails: AtomicU32,
    open_until: Mutex<Option<Instant>>,
    /// Exponential average of the latency of successful calls, in microseconds (0: none yet).
    latency_us: AtomicU64,
}

impl Health {
    fn new(breaker: Breaker) -> Health {
        Health { breaker, fails: AtomicU32::new(0), open_until: Mutex::new(None), latency_us: AtomicU64::new(0) }
    }

    /// Not in the cool-down of an open breaker. After the cool-down one call is let through; a failure opens the breaker again at once.
    fn available(&self) -> bool {
        match *self.open_until.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(t) => Instant::now() >= t,
            None => true,
        }
    }

    fn note<T>(&self, r: &Result<T>, took: Duration) {
        match r {
            Ok(_) => {
                self.fails.store(0, Ordering::Relaxed);
                *self.open_until.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX).max(1);
                let old = self.latency_us.load(Ordering::Relaxed);
                self.latency_us.store(if old == 0 { us } else { (old * 7 + us) / 8 }, Ordering::Relaxed);
            }
            // A missing or bad object is about the object, not about the backend.
            Err(Error::NotFound(_) | Error::Invalid(_) | Error::Conflict(_)) => {}
            Err(_) => {
                let n = self.fails.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                if n >= self.breaker.threshold {
                    *self.open_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + self.breaker.cooldown);
                }
            }
        }
    }
}

/// One backend of the pool as the pool uses it.
pub struct Member {
    id: BackendId,
    weight: u32,
    state: BackendState,
    store: Arc<dyn BlobStore>,
    health: Health,
}

impl Member {
    fn usable_for_write(&self) -> bool {
        self.state.writable() && self.health.available()
    }
}

/// What one run of [`PoolStore::repair`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Outstanding deletions carried out.
    pub deleted: u64,
    /// Copies made (to reach `copies`, or to move an object off a draining backend).
    pub copied: u64,
    /// Objects removed from a draining backend after enough copies exist elsewhere.
    pub drained: u64,
    /// Objects that could not be handled this time (no healthy holder, no healthy target, an error).
    pub failed: u64,
}

pub struct PoolStore {
    store: StoreId,
    policy: PoolPolicy,
    repo: Arc<dyn BlobPoolRepo>,
    members: Vec<Arc<Member>>,
    rotate: AtomicUsize,
    gc: Arc<tokio::sync::Mutex<()>>,
}

/// Rendezvous score: higher wins; proportional to `weight` over many objects.
fn score(hash: &BlobHash, id: BackendId, weight: u32) -> f64 {
    let mut h = Sha256::new();
    h.update(hash.as_str().as_bytes());
    h.update(id.0.to_le_bytes());
    let d = h.finalize();
    let mut eight = [0u8; 8];
    eight.copy_from_slice(d.get(..8).unwrap_or(&[0; 8]));
    // A number strictly between 0 and 1.
    let u = ((u64::from_le_bytes(eight) >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0;
    f64::from(weight) / -u.ln()
}

impl PoolStore {
    /// A pool over `backends` (the rows and the stores opened from them). At least one backend; the policy is checked against what the backends can do.
    pub fn new(store: StoreId, policy: PoolPolicy, repo: Arc<dyn BlobPoolRepo>, backends: Vec<(BackendRow, Arc<dyn BlobStore>)>) -> Result<PoolStore> {
        PoolStore::with_breaker(store, policy, repo, backends, Breaker::default())
    }

    pub fn with_breaker(store: StoreId, policy: PoolPolicy, repo: Arc<dyn BlobPoolRepo>, backends: Vec<(BackendRow, Arc<dyn BlobStore>)>, breaker: Breaker) -> Result<PoolStore> {
        policy.check()?;
        if backends.is_empty() || backends.len() > MAX_BACKENDS {
            return Err(Error::Invalid("a pool has one to eight backends".into()));
        }
        let members: Vec<Arc<Member>> = backends
            .into_iter()
            .map(|(row, store)| Arc::new(Member { id: row.id, weight: row.weight, state: row.state, store, health: Health::new(breaker) }))
            .collect();
        let writable = members.iter().filter(|m| m.state.writable()).count();
        if writable < policy.sync_copies as usize {
            return Err(Error::Busy("the pool has fewer active backends than sync_copies".into()));
        }
        Ok(PoolStore { store, policy, repo, members, rotate: AtomicUsize::new(0), gc: Arc::new(tokio::sync::Mutex::new(())) })
    }

    pub fn policy(&self) -> PoolPolicy {
        self.policy
    }

    fn member(&self, id: BackendId) -> Option<&Arc<Member>> {
        self.members.iter().find(|m| m.id == id)
    }

    /// The writable, healthy backends in placement order.
    fn placement_order(&self, hash: &BlobHash) -> Vec<Arc<Member>> {
        let mut v: Vec<Arc<Member>> = self.members.iter().filter(|m| m.usable_for_write()).cloned().collect();
        if self.policy.placement == Placement::Weighted {
            v.sort_by(|a, b| score(hash, b.id, b.weight).partial_cmp(&score(hash, a.id, a.weight)).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
        }
        v
    }

    async fn put_one(&self, m: &Arc<Member>, bytes: Bytes, hash: &BlobHash) -> Result<()> {
        put_one(self.repo.clone(), self.store, m.clone(), bytes, hash.clone()).await
    }

    /// Make the pool whole again: carry out outstanding deletions, copy objects that have fewer than `copies` copies, move objects off a draining backend. At most `batch` objects of each
    /// kind per call (it is meant to be called again until it reports nothing to do).
    pub async fn repair(&self, batch: u32) -> Result<RepairReport> {
        let mut report = RepairReport::default();
        // 1. Deletions that could not be carried out when the blob was collected.
        for (hash, backend) in self.repo.pending_deletes(self.store, batch).await? {
            let Some(m) = self.member(backend) else { continue };
            if !m.state.readable() || !m.health.available() {
                continue;
            }
            // An object younger than a minute is left for the next round: a put of the same bytes may be about to claim it (that put also cancels the deletion).
            let cutoff = SystemTime::now().checked_sub(Duration::from_secs(60)).unwrap_or(SystemTime::UNIX_EPOCH);
            let t = Instant::now();
            let r = m.store.delete_if_older(&hash, cutoff).await;
            m.health.note(&r, t.elapsed());
            match r {
                Ok(true) => {
                    self.repo.remove_location(self.store, &hash, backend).await?;
                    report.deleted += 1;
                }
                Ok(false) => {}
                Err(_) => report.failed += 1,
            }
        }
        // 2. Objects with too few copies (a backend was down for a write, a copy was bad, a backend was added or is draining).
        let mut after: Option<BlobHash> = None;
        let mut seen = 0u32;
        loop {
            let hashes = self.repo.under_replicated(self.store, self.policy.copies, after.as_ref(), batch.min(200)).await?;
            let Some(last) = hashes.last().cloned() else { break };
            after = Some(last);
            for hash in &hashes {
                match self.top_up(hash).await {
                    Ok(n) => report.copied += n,
                    Err(_) => report.failed += 1,
                }
            }
            seen = seen.saturating_add(u32::try_from(hashes.len()).unwrap_or(u32::MAX));
            if seen >= batch {
                break;
            }
        }
        // 3. Draining backends give their objects away once enough copies exist elsewhere.
        for m in self.members.iter().filter(|m| m.state == BackendState::Draining) {
            let mut after: Option<BlobHash> = None;
            let mut seen = 0u32;
            'drain: loop {
                let hashes = self.repo.held_by(m.id, after.as_ref(), batch.min(200)).await?;
                let Some(last) = hashes.last().cloned() else { break };
                after = Some(last);
                for hash in &hashes {
                    let locs = self.repo.locations(self.store, hash).await?;
                    let elsewhere = locs.iter().filter(|l| !l.pending_delete && self.member(l.backend).is_some_and(|o| o.id != m.id && matches!(o.state, BackendState::Active | BackendState::ReadOnly))).count();
                    if elsewhere < self.policy.copies as usize {
                        // Not yet: step 2 makes the copies first (an object whose copies cannot be made stays where it is).
                        continue;
                    }
                    let cutoff = SystemTime::now() + Duration::from_secs(3600);
                    let t = Instant::now();
                    let r = m.store.delete_if_older(hash, cutoff).await;
                    m.health.note(&r, t.elapsed());
                    match r {
                        Ok(_) => {
                            self.repo.remove_location(self.store, hash, m.id).await?;
                            report.drained += 1;
                        }
                        Err(_) => report.failed += 1,
                    }
                }
                seen = seen.saturating_add(u32::try_from(hashes.len()).unwrap_or(u32::MAX));
                if seen >= batch {
                    break 'drain;
                }
            }
        }
        Ok(report)
    }

    /// Bring one object up to `copies` copies on active or read-only backends; the number of copies made.
    async fn top_up(&self, hash: &BlobHash) -> Result<u64> {
        let locs = self.repo.locations(self.store, hash).await?;
        let counted = |id: BackendId| self.member(id).is_some_and(|m| matches!(m.state, BackendState::Active | BackendState::ReadOnly));
        let have: HashSet<BackendId> = locs.iter().filter(|l| !l.pending_delete).map(|l| l.backend).collect();
        let good = have.iter().filter(|b| counted(**b)).count();
        let need = (self.policy.copies as usize).saturating_sub(good);
        if need == 0 {
            return Ok(0);
        }
        let bytes = self.get(hash).await?;
        let mut made = 0u64;
        for m in self.placement_order(hash).into_iter().filter(|m| !have.contains(&m.id)) {
            if made as usize >= need {
                break;
            }
            if self.put_one(&m, bytes.clone(), hash).await.is_ok() {
                made += 1;
            }
        }
        if (made as usize) < need {
            return Err(Error::Busy("not enough healthy backends to make the copies".into()));
        }
        Ok(made)
    }
}

/// One copy: put, check the name, record the location.
async fn put_one(repo: Arc<dyn BlobPoolRepo>, store: StoreId, m: Arc<Member>, bytes: Bytes, hash: BlobHash) -> Result<()> {
    let t = Instant::now();
    let r = m.store.put(bytes).await;
    m.health.note(&r, t.elapsed());
    let got = r?;
    if got != hash {
        return Err(Error::storage("a backend stored the object under another name"));
    }
    repo.add_locations(store, &hash, &[m.id]).await
}

#[async_trait]
impl BlobStore for PoolStore {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash> {
        let hash = BlobHash::of(&bytes);
        let holders: HashSet<BackendId> = self.repo.locations(self.store, &hash).await?.into_iter().filter(|l| !l.pending_delete).map(|l| l.backend).collect();
        let mut order = self.placement_order(&hash);
        // The backends that already hold it first (stable: the placement order is kept within each group).
        order.sort_by_key(|m| !holders.contains(&m.id));
        let want = self.policy.copies as usize;
        let need = self.policy.sync_copies as usize;
        if order.len() < need {
            return Err(Error::Busy("not enough healthy backends".into()));
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<()>>();
        let (mut next, mut outstanding, mut ok) = (0usize, 0usize, 0usize);
        let mut last: Option<Error> = None;
        loop {
            // Keep `want` copies in flight or done; a copy that failed is replaced by the next candidate.
            while ok + outstanding < want {
                let Some(m) = order.get(next).cloned() else { break };
                next += 1;
                outstanding += 1;
                let (repo, store, bytes, h, tx) = (self.repo.clone(), self.store, bytes.clone(), hash.clone(), tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(put_one(repo, store, m, bytes, h).await);
                });
            }
            if ok >= need {
                return Ok(hash);
            }
            if outstanding == 0 {
                return Err(match last {
                    Some(Error::Busy(m)) => Error::Busy(m),
                    _ => Error::Busy("not enough backends accepted the object".into()),
                });
            }
            match rx.recv().await {
                Some(Ok(())) => {
                    ok += 1;
                    outstanding -= 1;
                }
                Some(Err(e)) => {
                    last = Some(e);
                    outstanding -= 1;
                }
                None => return Err(Error::Busy("the pool write was interrupted".into())),
            }
        }
    }

    async fn get(&self, h: &BlobHash) -> Result<Bytes> {
        let locs = self.repo.locations(self.store, h).await?;
        let mut holders: Vec<Arc<Member>> = locs.iter().filter(|l| !l.pending_delete).filter_map(|l| self.member(l.backend)).filter(|m| m.state.readable()).cloned().collect();
        match self.policy.read {
            ReadRule::Located | ReadRule::Ordered => {}
            ReadRule::RoundRobin => {
                if !holders.is_empty() {
                    let n = self.rotate.fetch_add(1, Ordering::Relaxed) % holders.len();
                    holders.rotate_left(n);
                }
            }
            ReadRule::Fastest => holders.sort_by_key(|m| m.health.latency_us.load(Ordering::Relaxed)),
        }
        // Backends whose breaker is open are tried only after every other holder failed.
        let (healthy, tripped): (Vec<_>, Vec<_>) = holders.into_iter().partition(|m| m.health.available());
        let mut tried: HashSet<BackendId> = HashSet::new();
        let mut last: Option<Error> = None;
        for m in healthy.into_iter().chain(tripped) {
            tried.insert(m.id);
            let t = Instant::now();
            let r = m.store.get(h).await;
            m.health.note(&r, t.elapsed());
            match r {
                Ok(b) if BlobHash::of(&b) == *h => return Ok(b),
                Ok(_) => {
                    // A bad copy: remove it (so that a new copy is really written: a put of content that exists does not rewrite it) and forget it, so that repair makes one.
                    let _ = m.store.delete_if_older(h, SystemTime::now() + Duration::from_secs(3600)).await;
                    let _ = self.repo.remove_location(self.store, h, m.id).await;
                    last = Some(Error::storage("a stored blob does not match its name"));
                }
                Err(Error::NotFound(_)) => {
                    let _ = self.repo.remove_location(self.store, h, m.id).await;
                }
                Err(e) => last = Some(e),
            }
        }
        // No recorded holder had it: it may be there without a record (the record of a copy could not be written). Look everywhere once.
        if locs.iter().all(|l| l.pending_delete) {
            for m in self.members.iter().filter(|m| m.state.readable() && !tried.contains(&m.id)) {
                let t = Instant::now();
                let r = m.store.get(h).await;
                m.health.note(&r, t.elapsed());
                match r {
                    Ok(b) if BlobHash::of(&b) == *h => {
                        let _ = self.repo.add_locations(self.store, h, &[m.id]).await;
                        return Ok(b);
                    }
                    Ok(_) | Err(Error::NotFound(_)) => {}
                    Err(e) => last = Some(e),
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::NotFound("blob".into())))
    }

    async fn list_shard(&self, shard: u8) -> Result<Vec<BlobEntry>> {
        let mut by_hash: HashMap<BlobHash, BlobEntry> = HashMap::new();
        let mut answered = false;
        let mut last: Option<Error> = None;
        for m in self.members.iter().filter(|m| m.state.readable()) {
            match m.store.list_shard(shard).await {
                Ok(entries) => {
                    answered = true;
                    for e in entries {
                        // The youngest copy decides: an object is as old as its most recent touch anywhere.
                        match by_hash.get_mut(&e.hash) {
                            Some(old) if old.modified_unix >= e.modified_unix => {}
                            Some(old) => *old = e,
                            None => {
                                by_hash.insert(e.hash.clone(), e);
                            }
                        }
                    }
                }
                Err(e) => last = Some(e),
            }
        }
        if !answered {
            return Err(last.unwrap_or_else(|| Error::storage("the pool has no backend to list")));
        }
        let mut out: Vec<BlobEntry> = by_hash.into_values().collect();
        out.sort_by(|a, b| a.hash.as_str().cmp(b.hash.as_str()));
        Ok(out)
    }

    async fn delete_if_older(&self, h: &BlobHash, cutoff: SystemTime) -> Result<bool> {
        let locs = self.repo.locations(self.store, h).await?;
        let mut deleted = false;
        let mut failed: Option<Error> = None;
        let mut unreachable = false;
        for m in &self.members {
            let located = locs.iter().any(|l| l.backend == m.id);
            if !m.state.readable() {
                unreachable |= located;
                continue;
            }
            let t = Instant::now();
            let r = m.store.delete_if_older(h, cutoff).await;
            m.health.note(&r, t.elapsed());
            match r {
                Ok(true) => {
                    deleted = true;
                    self.repo.remove_location(self.store, h, m.id).await?;
                }
                Ok(false) => {}
                Err(e) => {
                    unreachable |= located;
                    failed = Some(e);
                }
            }
        }
        unreachable |= locs.iter().any(|l| self.member(l.backend).is_none());
        if unreachable {
            // The blob row is gone: what is left on a backend that could not be reached is deleted later, by repair.
            self.repo.mark_all_pending_delete(self.store, h).await?;
        }
        match (deleted, failed) {
            (false, Some(e)) => Err(e),
            _ => Ok(deleted),
        }
    }

    async fn sweep_temp_before(&self, cutoff: SystemTime) -> Result<u64> {
        let (mut total, mut answered, mut last) = (0u64, false, None);
        for m in self.members.iter().filter(|m| m.state.readable()) {
            match m.store.sweep_temp_before(cutoff).await {
                Ok(n) => {
                    answered = true;
                    total += n;
                }
                Err(e) => last = Some(e),
            }
        }
        match (answered, last) {
            (false, Some(e)) => Err(e),
            _ => Ok(total),
        }
    }

    async fn heal_quarantine(&self, cutoff: SystemTime) -> Result<HealReport> {
        let (mut total, mut answered, mut last) = (HealReport::default(), false, None);
        for m in self.members.iter().filter(|m| m.state.readable()) {
            match m.store.heal_quarantine(cutoff).await {
                Ok(r) => {
                    answered = true;
                    total.restored += r.restored;
                    total.removed += r.removed;
                    total.failed += r.failed;
                }
                Err(e) => last = Some(e),
            }
        }
        match (answered, last) {
            (false, Some(e)) => Err(e),
            _ => Ok(total),
        }
    }

    async fn try_lock_gc(&self) -> Result<GcLock> {
        let guard = match self.gc.clone().try_lock_owned() {
            Ok(g) => g,
            Err(_) => return Err(Error::Conflict("gc already running".into())),
        };
        // Every backend's own lock too (a filesystem backend's lock is shared with other processes). One that cannot be taken is `Conflict`.
        let mut locks = Vec::new();
        for m in self.members.iter().filter(|m| m.state.readable()) {
            locks.push(m.store.try_lock_gc().await?);
        }
        Ok(GcLock::held((guard, locks)))
    }

    async fn maintain(&self, batch: u32) -> Result<u64> {
        let r = self.repair(batch).await?;
        Ok(r.deleted + r.copied + r.drained)
    }

    fn fs_now(&self) -> SystemTime {
        self.members.iter().find(|m| m.state.readable()).map_or_else(SystemTime::now, |m| m.store.fs_now())
    }
}
