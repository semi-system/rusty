//! Blob pools (migration 0024, docs/design-s3-per-tenant.md, section 12): JBOD, mirror and failover over several backends, with locations, a breaker, repair, draining and
//! garbage collection that reaches every copy. The backends here are directories (one of them can be "stopped"); the S3 backends are the same code behind the same trait.
mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use common::backends::*;
use common::raw;
use rusty_core::{BlobHash, DomainId, Error, Flags, Result};
use rusty_store::repo::{BackendId, BackendRow, BackendState, BlobPoolRepo, BlobStoreRepo, StoreId, MAX_BACKENDS};
use rusty_store::{BackendOpener, BlobEntry, BlobRouter, BlobStore, BlobStores, Breaker, FsStoreOpener, GcLock, HealReport, KindOpener, Mailbox, PoolOpener, PoolPolicy, PoolStore, StoreOpener};

const HOUR: Duration = Duration::from_secs(3600);

/// A directory backend that can be stopped: every call fails while it is down.
struct Flaky {
    inner: Arc<dyn BlobStore>,
    down: Arc<AtomicBool>,
}

impl Flaky {
    fn check(&self) -> Result<()> {
        if self.down.load(Ordering::SeqCst) {
            Err(Error::storage("the backend is down"))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl BlobStore for Flaky {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash> {
        self.check()?;
        self.inner.put(bytes).await
    }
    async fn get(&self, h: &BlobHash) -> Result<Bytes> {
        self.check()?;
        self.inner.get(h).await
    }
    async fn list_shard(&self, shard: u8) -> Result<Vec<BlobEntry>> {
        self.check()?;
        self.inner.list_shard(shard).await
    }
    async fn delete_if_older(&self, h: &BlobHash, cutoff: SystemTime) -> Result<bool> {
        self.check()?;
        self.inner.delete_if_older(h, cutoff).await
    }
    async fn sweep_temp_before(&self, cutoff: SystemTime) -> Result<u64> {
        self.check()?;
        self.inner.sweep_temp_before(cutoff).await
    }
    async fn heal_quarantine(&self, cutoff: SystemTime) -> Result<HealReport> {
        self.check()?;
        self.inner.heal_quarantine(cutoff).await
    }
    async fn try_lock_gc(&self) -> Result<GcLock> {
        self.inner.try_lock_gc().await
    }
    fn fs_now(&self) -> SystemTime {
        self.inner.fs_now()
    }
}

/// Opens the directory backends under one base directory, each behind a switch named by its directory.
struct Backends {
    fs: FsStoreOpener,
    base: std::path::PathBuf,
    switches: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Backends {
    fn new(base: std::path::PathBuf) -> Arc<Backends> {
        Arc::new(Backends { fs: FsStoreOpener::new(base.clone()), base, switches: Mutex::new(HashMap::new()) })
    }

    fn switch(&self, dir: &str) -> Arc<AtomicBool> {
        self.switches.lock().unwrap().entry(dir.to_owned()).or_default().clone()
    }

    fn stop(&self, dir: &str) {
        self.switch(dir).store(true, Ordering::SeqCst);
    }

    fn start(&self, dir: &str) {
        self.switch(dir).store(false, Ordering::SeqCst);
    }

    fn path(&self, dir: &str, h: &BlobHash) -> std::path::PathBuf {
        let s = h.as_str();
        self.base.join(dir).join(&s[0..2]).join(&s[2..4]).join(s)
    }

    fn holds(&self, dir: &str, h: &BlobHash) -> bool {
        self.path(dir, h).exists()
    }
}

#[async_trait]
impl BackendOpener for Backends {
    async fn open(&self, row: &BackendRow) -> Result<Arc<dyn BlobStore>> {
        let v: serde_json::Value = serde_json::from_str(&row.config).unwrap();
        let dir = v["dir"].as_str().unwrap().to_owned();
        let inner = BackendOpener::open(&self.fs, row).await?;
        Ok(Arc::new(Flaky { inner, down: self.switch(&dir) }))
    }
}

struct Rig {
    b: Backend,
    repo: Arc<rusty_store::repo::pg::PgRepo>,
    be: Arc<Backends>,
    domain: DomainId,
    store: StoreId,
    _tmp: tempfile::TempDir,
}

/// A domain with a pool store of the given policy and one directory backend per `(name, weight)`, all checked.
async fn rig(policy: &str, backends: &[(&str, u32)]) -> Option<Rig> {
    let b = pg_backend().await?;
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let tmp = tempfile::tempdir().unwrap();
    let be = Backends::new(tmp.path().join("pool"));
    let domain = b.dir.create_domain("pool.test").await.unwrap();
    let store = repo.create_for_domain(domain, "pool", policy).await.unwrap();
    for (name, weight) in backends {
        let id = repo.add_backend(store, name, "fs", &format!(r#"{{"dir":"{name}"}}"#), *weight).await.unwrap();
        repo.record_backend_check(id, "ok", None).await.unwrap();
    }
    Some(Rig { b, repo, be, domain, store, _tmp: tmp })
}

impl Rig {
    /// Put an object and give it the blob row a message would have made (the repair job only looks after objects that have one).
    async fn put(&self, pool: &Arc<dyn BlobStore>, i: usize) -> BlobHash {
        let h = pool.put(body(i)).await.unwrap();
        sqlx::query("INSERT INTO blobs (store_id, hash, size, refcount) VALUES ($1, decode($2, 'hex'), 10, 1) ON CONFLICT DO NOTHING").bind(self.store.0).bind(h.as_str()).execute(self.b.fx()).await.unwrap();
        h
    }

    async fn pool(&self) -> Arc<dyn BlobStore> {
        self.pool_with(Breaker::default()).await
    }

    async fn pool_with(&self, breaker: Breaker) -> Arc<dyn BlobStore> {
        let row = self.repo.get(self.store).await.unwrap().unwrap();
        let rows: Vec<BackendRow> = BlobPoolRepo::backends(&*self.repo, self.store).await.unwrap().into_iter().filter(|r| r.state != BackendState::Disabled).collect();
        let mut opened = Vec::new();
        for r in rows {
            let s = BackendOpener::open(&*self.be, &r).await.unwrap();
            opened.push((r, s));
        }
        Arc::new(PoolStore::with_breaker(self.store, PoolPolicy::parse(&row.config).unwrap(), self.repo.clone(), opened, breaker).unwrap())
    }

    async fn id_of(&self, name: &str) -> BackendId {
        BlobPoolRepo::backends(&*self.repo, self.store).await.unwrap().into_iter().find(|r| r.name == name).unwrap().id
    }

    async fn name_of(&self, id: BackendId) -> String {
        BlobPoolRepo::backend(&*self.repo, id).await.unwrap().unwrap().name
    }

    async fn holders(&self, h: &BlobHash) -> Vec<String> {
        let mut out = Vec::new();
        for l in BlobPoolRepo::locations(&*self.repo, self.store, h).await.unwrap() {
            assert!(!l.pending_delete);
            out.push(self.name_of(l.backend).await);
        }
        out.sort();
        out
    }

    async fn wait_holders(&self, h: &BlobHash, n: usize) {
        for _ in 0..200 {
            if self.holders(h).await.len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the object never got {n} locations: {:?}", self.holders(h).await);
    }
}

fn body(i: usize) -> Bytes {
    Bytes::from(format!("body number {i} of the pool test, long enough to be a message").into_bytes())
}

// ------------------------------------------------------------------------------------------------ policy

#[test]
fn the_policy_is_validated_and_a_mirror_needs_no_sync_setting() {
    let d = PoolPolicy::parse("{}").unwrap();
    assert_eq!((d.copies, d.sync_copies), (1, 1), "the default is JBOD");
    let m = PoolPolicy::parse(r#"{"copies":3}"#).unwrap();
    assert_eq!((m.copies, m.sync_copies), (3, 3), "copies alone is a mirror");
    let f = PoolPolicy::parse(r#"{"copies":2,"sync_copies":1,"placement":"ordered","read":"ordered"}"#).unwrap();
    assert_eq!((f.copies, f.sync_copies), (2, 1));
    assert_eq!(PoolPolicy::parse(&f.to_json()).unwrap(), f, "what is written reads back");
    for bad in [
        "not json", "[]", r#"{"copies":0}"#, r#"{"copies":9}"#, r#"{"copies":2,"sync_copies":3}"#, r#"{"sync_copies":0}"#, r#"{"placement":"random"}"#, r#"{"read":"x"}"#, r#"{"unknown":1}"#,
        r#"{"copies":"2"}"#, r#"{"copies":-1}"#,
    ] {
        assert!(PoolPolicy::parse(bad).is_err(), "{bad}");
    }
}

// ------------------------------------------------------------------------------------------------ the backends of a store

#[tokio::test]
async fn a_pool_has_at_most_eight_backends_with_their_own_names_and_only_a_pool_has_any() {
    let Some(r) = rig("{}", &[("a", 100)]).await else { return };
    assert!(matches!(r.repo.add_backend(r.store, "a", "fs", r#"{"dir":"a"}"#, 100).await, Err(Error::Conflict(_))), "a name is used once per store");
    for i in 1..MAX_BACKENDS {
        r.repo.add_backend(r.store, &format!("b{i}"), "fs", &format!(r#"{{"dir":"b{i}"}}"#), 100).await.unwrap();
    }
    assert!(matches!(r.repo.add_backend(r.store, "one-too-many", "fs", r#"{"dir":"x"}"#, 100).await, Err(Error::Conflict(_))));
    assert_eq!(BlobPoolRepo::backends(&*r.repo, r.store).await.unwrap().len(), MAX_BACKENDS);
    // A store that is not a pool takes no backend.
    let other = r.b.dir.create_domain("plain.test").await.unwrap();
    let plain = r.repo.create_for_domain(other, "fs", r#"{"dir":"plain"}"#).await.unwrap();
    assert!(matches!(r.repo.add_backend(plain, "a", "fs", r#"{"dir":"a"}"#, 100).await, Err(Error::NotFound(_))));
    assert!(matches!(r.repo.add_backend(StoreId(1), "a", "fs", r#"{"dir":"a"}"#, 100).await, Err(Error::NotFound(_))), "nor the platform store");
    for bad in ["", "UPPER", "-lead", "has space", &"x".repeat(33)] {
        assert!(r.repo.add_backend(r.store, bad, "fs", "{}", 100).await.is_err(), "{bad:?}");
    }
    let _ = r.domain;
}

#[tokio::test]
async fn a_pool_is_opened_only_when_every_backend_passed_a_check() {
    let Some(r) = rig("{}", &[("a", 100)]).await else { return };
    let id = r.repo.add_backend(r.store, "b", "fs", r#"{"dir":"b"}"#, 100).await.unwrap();
    let opener = PoolOpener::new(r.repo.clone(), r.be.clone());
    let row = r.repo.get(r.store).await.unwrap().unwrap();
    assert!(matches!(opener.open(&row).await, Err(Error::Busy(_))), "a backend nobody has seen work");
    r.repo.record_backend_check(id, "auth", None).await.unwrap();
    assert!(matches!(opener.open(&row).await, Err(Error::Busy(_))), "a backend whose check failed");
    r.repo.record_backend_check(id, "ok", None).await.unwrap();
    assert!(opener.open(&row).await.is_ok());
    // A disabled backend does not count; changing the settings of a backend resets its check.
    r.repo.set_backend_state(id, BackendState::Disabled).await.unwrap();
    r.repo.update_backend_config(id, r#"{"dir":"c"}"#).await.unwrap();
    assert_eq!(r.repo.backend(id).await.unwrap().unwrap().check_status, "unchecked");
    assert!(opener.open(&row).await.is_ok(), "the disabled backend is not asked");
    // A pool without any usable backend is refused.
    let empty_dom = r.b.dir.create_domain("empty.test").await.unwrap();
    let empty = r.repo.create_for_domain(empty_dom, "pool", "{}").await.unwrap();
    let erow = r.repo.get(empty).await.unwrap().unwrap();
    assert!(matches!(opener.open(&erow).await, Err(Error::Invalid(_))));
}

// ------------------------------------------------------------------------------------------------ JBOD

#[tokio::test]
async fn jbod_keeps_each_object_on_one_backend_chosen_by_weight_and_always_the_same_one() {
    let Some(r) = rig(r#"{"copies":1,"placement":"weighted","read":"located"}"#, &[("heavy", 1000), ("mid", 300), ("light", 100)]).await else { return };
    let pool = r.pool().await;
    let mut count: HashMap<String, usize> = HashMap::new();
    let mut hashes = Vec::new();
    for i in 0..120 {
        let h = r.put(&pool, i).await;
        let holders = r.holders(&h).await;
        assert_eq!(holders.len(), 1, "one copy");
        assert!(r.be.holds(&holders[0], &h) && ["heavy", "mid", "light"].iter().filter(|n| **n != holders[0]).all(|n| !r.be.holds(n, &h)), "only on its backend");
        *count.entry(holders[0].clone()).or_default() += 1;
        hashes.push((h, holders[0].clone()));
    }
    assert!(count["heavy"] > count["mid"] && count["mid"] >= count["light"], "capacity adds up by weight: {count:?}");
    assert!(count.len() == 3, "every backend is used: {count:?}");
    // A second pool object over the same rows places the same way, and every object reads back.
    let again = r.pool().await;
    for (i, (h, on)) in hashes.iter().enumerate() {
        assert_eq!(again.put(body(i)).await.unwrap(), *h);
        assert_eq!(&r.holders(h).await, &vec![on.clone()], "a repeated put stays where the object is");
        assert_eq!(again.get(h).await.unwrap(), body(i));
    }
    // Losing a backend loses its objects only, and says so for those alone.
    r.be.stop("light");
    let (lost, kept): (Vec<_>, Vec<_>) = hashes.iter().partition(|(_, on)| on == "light");
    for (h, _) in &lost {
        assert!(again.get(h).await.is_err(), "no copy elsewhere");
    }
    for (h, _) in &kept {
        assert!(again.get(h).await.is_ok(), "the others are unaffected");
    }
}

// ------------------------------------------------------------------------------------------------ mirror

#[tokio::test]
async fn a_mirror_writes_every_copy_before_it_answers_and_survives_a_stopped_backend() {
    let Some(r) = rig(r#"{"copies":2,"read":"round_robin"}"#, &[("a", 100), ("b", 100), ("c", 100)]).await else { return };
    let pool = r.pool_with(Breaker { threshold: 2, cooldown: Duration::from_secs(60) }).await;
    let h = r.put(&pool, 1).await;
    assert_eq!(r.holders(&h).await.len(), 2, "two copies are recorded when the put returns");
    for n in r.holders(&h).await {
        assert!(r.be.holds(&n, &h));
    }
    // One holder stops: the object is still read (from the other), and a new object still gets two copies on the two that work.
    let holders = r.holders(&h).await;
    r.be.stop(&holders[0]);
    for _ in 0..4 {
        assert_eq!(pool.get(&h).await.unwrap(), body(1));
    }
    let h2 = r.put(&pool, 2).await;
    let on = r.holders(&h2).await;
    assert_eq!(on.len(), 2);
    assert!(!on.contains(&holders[0]), "the stopped backend holds no new copy: {on:?}");
    // Two stopped out of three: two copies are impossible, the put is a temporary failure (the sender retries) and nothing is half written as a success.
    let other = ["a", "b", "c"].into_iter().find(|n| *n != holders[0]).unwrap();
    r.be.stop(other);
    assert!(matches!(pool.put(body(3)).await, Err(Error::Busy(_))), "fewer than sync_copies healthy backends");
    // They come back: writes work again once their breakers allow a call (the cool-down is long here, so use a fresh pool).
    r.be.start(&holders[0]);
    r.be.start(other);
    let fresh = r.pool().await;
    assert!(fresh.put(body(3)).await.is_ok());
}

#[tokio::test]
async fn a_bad_copy_is_never_returned_it_is_forgotten_and_repair_makes_a_new_one() {
    let Some(r) = rig(r#"{"copies":2,"read":"ordered","placement":"ordered"}"#, &[("a", 100), ("b", 100), ("c", 100)]).await else { return };
    let pool = r.pool().await;
    let h = r.put(&pool, 7).await;
    assert_eq!(r.holders(&h).await, vec!["a".to_owned(), "b".to_owned()]);
    // The copy on `a` is replaced by other bytes: the read goes on to `b`.
    std::fs::write(r.be.path("a", &h), b"somebody else's mail").unwrap();
    assert_eq!(pool.get(&h).await.unwrap(), body(7));
    assert_eq!(r.holders(&h).await, vec!["b".to_owned()], "the bad copy is forgotten");
    // Repair copies it to a healthy backend that does not hold it (`a` or `c`).
    let report = r.pool_repair(&pool).await;
    assert_eq!(report.copied, 1);
    assert_eq!(r.holders(&h).await.len(), 2);
    assert_eq!(pool.get(&h).await.unwrap(), body(7));
    // A missing object at a recorded location is forgotten the same way.
    let on = r.holders(&h).await;
    std::fs::remove_file(r.be.path(&on[0], &h)).unwrap();
    assert_eq!(pool.get(&h).await.unwrap(), body(7));
    assert_eq!(r.holders(&h).await.len(), 1);
}

impl Rig {
    async fn pool_repair(&self, _pool: &Arc<dyn BlobStore>) -> rusty_store::RepairReport {
        // `repair` is a method of the concrete store: build one over the same rows.
        let row = self.repo.get(self.store).await.unwrap().unwrap();
        let rows = BlobPoolRepo::backends(&*self.repo, self.store).await.unwrap();
        let mut opened = Vec::new();
        for r in rows.into_iter().filter(|r| r.state != BackendState::Disabled) {
            let s = BackendOpener::open(&*self.be, &r).await.unwrap();
            opened.push((r, s));
        }
        PoolStore::new(self.store, PoolPolicy::parse(&row.config).unwrap(), self.repo.clone(), opened).unwrap().repair(1000).await.unwrap()
    }
}

// ------------------------------------------------------------------------------------------------ failover

#[tokio::test]
async fn failover_answers_after_one_copy_and_fills_the_others_afterwards_and_by_repair() {
    let Some(r) = rig(r#"{"copies":2,"sync_copies":1,"placement":"ordered","read":"ordered"}"#, &[("primary", 100), ("standby", 100)]).await else { return };
    let pool = r.pool().await;
    let h = r.put(&pool, 1).await;
    r.wait_holders(&h, 2).await; // the second copy finishes in the background
    assert!(r.be.holds("primary", &h) && r.be.holds("standby", &h));

    // The standby is down for a write: the put still succeeds on the primary, with one location; repair makes the second copy when the standby is back.
    r.be.stop("standby");
    let h2 = r.put(&pool, 2).await;
    assert_eq!(r.holders(&h2).await, vec!["primary".to_owned()]);
    r.be.start("standby");
    let under = BlobPoolRepo::under_replicated(&*r.repo, r.store, 2, None, 100).await.unwrap();
    assert_eq!(under, vec![h2.clone()], "exactly the object written while the standby was down");
    let report = r.pool_repair(&pool).await;
    assert_eq!(report.copied, 1);
    assert!(r.be.holds("standby", &h2));
    assert!(BlobPoolRepo::under_replicated(&*r.repo, r.store, 2, None, 100).await.unwrap().is_empty());

    // The primary dies: reads come from the standby; a write has one backend left, which is enough for sync_copies = 1.
    r.be.stop("primary");
    assert_eq!(pool.get(&h).await.unwrap(), body(1));
    let h3 = r.put(&pool, 3).await;
    assert_eq!(r.holders(&h3).await, vec!["standby".to_owned()]);
    // Both down: reads and writes fail as temporary errors for those objects, not as missing mail.
    r.be.stop("standby");
    assert!(pool.get(&h).await.is_err());
    assert!(matches!(pool.put(body(4)).await, Err(Error::Busy(_))));
}

#[tokio::test]
async fn a_backend_that_keeps_failing_is_left_out_by_its_breaker_until_it_recovers() {
    let Some(r) = rig(r#"{"copies":1,"placement":"ordered"}"#, &[("a", 100), ("b", 100)]).await else { return };
    let pool = r.pool_with(Breaker { threshold: 3, cooldown: Duration::from_millis(400) }).await;
    r.be.stop("a");
    // The first puts try `a` (it is first), fail and fall back to `b`; after three failures `a` is not tried at all.
    for i in 0..3 {
        let h = r.put(&pool, i).await;
        assert_eq!(r.holders(&h).await, vec!["b".to_owned()]);
    }
    r.be.start("a");
    let h = r.put(&pool, 10).await;
    assert_eq!(r.holders(&h).await, vec!["b".to_owned()], "its breaker is open: not chosen although it works again");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let h = r.put(&pool, 11).await;
    assert_eq!(r.holders(&h).await, vec!["a".to_owned()], "after the cool-down it is tried again and works");
}

// ------------------------------------------------------------------------------------------------ deletion and repair

#[tokio::test]
async fn deleting_reaches_every_copy_and_a_backend_that_was_down_is_cleaned_by_repair() {
    let Some(r) = rig(r#"{"copies":2,"placement":"ordered"}"#, &[("a", 100), ("b", 100)]).await else { return };
    let pool = r.pool().await;
    let (h1, h2) = (r.put(&pool, 1).await, r.put(&pool, 2).await);
    let later = SystemTime::now() + HOUR;
    assert!(pool.delete_if_older(&h1, later).await.unwrap());
    assert!(!r.be.holds("a", &h1) && !r.be.holds("b", &h1));
    assert!(BlobPoolRepo::locations(&*r.repo, r.store, &h1).await.unwrap().is_empty(), "no location is left");
    assert!(!pool.delete_if_older(&h1, later).await.unwrap(), "a second delete finds nothing");

    // `b` is down when the second is collected: the delete succeeds on `a`, `b` keeps a pending deletion.
    r.be.stop("b");
    assert!(pool.delete_if_older(&h2, later).await.unwrap());
    assert!(!r.be.holds("a", &h2) && r.be.holds("b", &h2));
    let pending = BlobPoolRepo::pending_deletes(&*r.repo, r.store, 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, h2);
    assert!(pool.get(&h2).await.is_err(), "a deleted blob is not served from the backend that missed the delete");
    // Repair while it is still down: nothing happens, nothing is lost. Back up: the object goes (the cutoff of repair is a minute ago, so age the file).
    assert_eq!(r.pool_repair(&pool).await.deleted, 0);
    r.be.start("b");
    let old = SystemTime::now() - Duration::from_secs(600);
    std::fs::File::options().write(true).open(r.be.path("b", &h2)).unwrap().set_modified(old).unwrap();
    assert_eq!(r.pool_repair(&pool).await.deleted, 1);
    assert!(!r.be.holds("b", &h2));
    assert!(BlobPoolRepo::pending_deletes(&*r.repo, r.store, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_put_of_the_same_bytes_cancels_a_pending_deletion() {
    let Some(r) = rig(r#"{"copies":2,"placement":"ordered"}"#, &[("a", 100), ("b", 100)]).await else { return };
    let pool = r.pool().await;
    let h = r.put(&pool, 5).await;
    r.be.stop("b");
    assert!(pool.delete_if_older(&h, SystemTime::now() + HOUR).await.unwrap());
    r.be.start("b");
    // The same message arrives again before repair ran: both copies are made again and the old deletion no longer applies.
    let fresh = r.pool().await;
    assert_eq!(fresh.put(body(5)).await.unwrap(), h);
    assert_eq!(r.holders(&h).await.len(), 2);
    assert!(BlobPoolRepo::pending_deletes(&*r.repo, r.store, 10).await.unwrap().is_empty());
    assert_eq!(r.pool_repair(&pool).await.deleted, 0);
    assert!(r.be.holds("b", &h));
}

#[tokio::test]
async fn listing_is_the_union_of_the_backends_and_an_object_without_a_record_is_still_found_and_collected() {
    let Some(r) = rig(r#"{"copies":1,"placement":"ordered"}"#, &[("a", 100), ("b", 100)]).await else { return };
    let pool = r.pool().await;
    let h = r.put(&pool, 1).await;
    // An object that landed on `b` without a recorded location (the record could not be written): a stray.
    let stray_bytes = body(99);
    let stray = BlobHash::of(&stray_bytes);
    let b_store = BackendOpener::open(&*r.be, &BlobPoolRepo::backends(&*r.repo, r.store).await.unwrap()[1]).await.unwrap();
    b_store.put(stray_bytes.clone()).await.unwrap();
    let mut all = pool.list().await.unwrap().into_iter().map(|e| e.hash).collect::<Vec<_>>();
    all.sort();
    let mut want = vec![h.clone(), stray.clone()];
    want.sort();
    assert_eq!(all, want, "every object of every backend is listed once");
    // Without a record the read still finds it, and records it.
    assert_eq!(pool.get(&stray).await.unwrap(), stray_bytes);
    assert_eq!(r.holders(&stray).await, vec!["b".to_owned()]);
    // Collection: the stray is deleted wherever it is, the pool's lock excludes a second collector.
    let lock = pool.try_lock_gc().await.unwrap();
    assert!(matches!(pool.try_lock_gc().await, Err(Error::Conflict(_))));
    drop(lock);
    assert!(pool.delete_if_older(&stray, SystemTime::now() + HOUR).await.unwrap());
    assert!(!r.be.holds("b", &stray));
}

// ------------------------------------------------------------------------------------------------ draining

#[tokio::test]
async fn a_draining_backend_gives_its_objects_away_and_can_then_be_removed() {
    let Some(r) = rig(r#"{"copies":1,"placement":"weighted"}"#, &[("old", 100), ("new1", 100), ("new2", 100)]).await else { return };
    let pool = r.pool().await;
    let mut hashes = Vec::new();
    for i in 0..40 {
        hashes.push(r.put(&pool, i).await);
    }
    let old = r.id_of("old").await;
    let on_old = BlobPoolRepo::count_held(&*r.repo, old).await.unwrap();
    assert!(on_old > 0, "some objects are on the backend to be retired");
    assert!(matches!(r.repo.remove_backend(old).await, Err(Error::Conflict(_))), "not while it holds objects");
    assert!(matches!(r.repo.update_backend_config(old, r#"{"dir":"elsewhere"}"#).await, Err(Error::Conflict(_))));
    r.repo.set_backend_state(old, BackendState::Draining).await.unwrap();
    // The repair job moves them: copies elsewhere first, then the objects go from the old backend.
    for _ in 0..5 {
        let report = r.pool_repair(&pool).await;
        if report.copied == 0 && report.drained == 0 {
            break;
        }
    }
    assert_eq!(BlobPoolRepo::count_held(&*r.repo, old).await.unwrap(), 0, "nothing left on the old backend");
    for (i, h) in hashes.iter().enumerate() {
        assert!(!r.be.holds("old", h));
        let fresh = r.pool().await;
        assert_eq!(fresh.get(h).await.unwrap(), body(i), "every object is on a new backend");
    }
    r.repo.remove_backend(old).await.unwrap();
    assert_eq!(BlobPoolRepo::backends(&*r.repo, r.store).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_read_only_backend_is_read_but_not_written() {
    let Some(r) = rig(r#"{"copies":1,"placement":"ordered"}"#, &[("a", 100), ("b", 100)]).await else { return };
    let pool = r.pool().await;
    let h = r.put(&pool, 1).await;
    assert_eq!(r.holders(&h).await, vec!["a".to_owned()]);
    r.repo.set_backend_state(r.id_of("a").await, BackendState::ReadOnly).await.unwrap();
    let pool = r.pool().await;
    assert_eq!(pool.get(&h).await.unwrap(), body(1), "still read");
    let h2 = r.put(&pool, 2).await;
    assert_eq!(r.holders(&h2).await, vec!["b".to_owned()], "new objects go to the other");
    // A pool with no writable backend cannot be opened at all.
    r.repo.set_backend_state(r.id_of("b").await, BackendState::ReadOnly).await.unwrap();
    let row = r.repo.get(r.store).await.unwrap().unwrap();
    let opener = PoolOpener::new(r.repo.clone(), r.be.clone());
    assert!(matches!(opener.open(&row).await, Err(Error::Busy(_))));
}

// ------------------------------------------------------------------------------------------------ through the mailbox and GC

#[tokio::test]
async fn a_tenant_on_a_pool_stores_reads_and_collects_its_mail_through_the_whole_stack() {
    let Some(r) = rig(r#"{"copies":2,"placement":"weighted","read":"fastest"}"#, &[("a", 100), ("b", 100), ("c", 100)]).await else { return };
    let repo = r.repo.clone();
    let opener = KindOpener::new().with_pool(Arc::new(PoolOpener::new(repo.clone(), r.be.clone())));
    let router: Arc<dyn BlobRouter> = Arc::new(BlobStores::new(r.b.blobs.clone(), repo.clone(), Arc::new(opener)));
    let acc = r.b.dir.create_account("alice", r.domain, None, None).await.unwrap();
    let mb = Mailbox::open_with(r.b.mrepo.clone(), router.clone(), acc).await.unwrap();
    let text = raw("mail on a pool", Some("<pool@x>"));
    let m = mb.append(INBOX, text.clone(), Flags::default()).await.unwrap();
    assert_eq!(mb.get_raw(m.id).await.unwrap(), text);
    let reference = mb.get_raw_ref(m.id).await.unwrap();
    assert_eq!(reference.store, r.store);
    assert_eq!(r.holders(&reference.blob).await.len(), 2, "two copies of a message of this tenant");
    // One backend lost: the message is still there.
    for n in r.holders(&reference.blob).await.iter().take(1) {
        r.be.stop(n);
    }
    assert_eq!(mb.get_raw(m.id).await.unwrap(), text);
    for n in ["a", "b", "c"] {
        r.be.start(n);
    }
    // Expunge, purge, collect: every copy goes, through the same GC as any store.
    mb.expunge_many_at(&[m.id], 1).await.unwrap();
    let gc = rusty_store::Gc::new(r.b.grepo.clone());
    gc.purge_deleted(i64::MAX).await.unwrap();
    let pool = router.store(r.store).await.unwrap();
    let report = gc.for_store(r.store).collect_blobs_at(&*pool, HOUR, SystemTime::now() + 2 * HOUR).await.unwrap();
    assert_eq!(report.rows_deleted, 1);
    for n in ["a", "b", "c"] {
        assert!(!r.be.holds(n, &reference.blob), "no copy is left on {n}");
    }
    assert!(BlobPoolRepo::locations(&*repo, r.store, &reference.blob).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_opened_store_is_read_again_after_the_refresh_time_so_a_changed_pool_takes_effect_without_a_restart() {
    let Some(r) = rig(r#"{"copies":1,"placement":"ordered"}"#, &[("a", 100)]).await else { return };
    let opener = KindOpener::new().with_pool(Arc::new(PoolOpener::new(r.repo.clone(), r.be.clone())));
    let router = BlobStores::new(r.b.blobs.clone(), r.repo.clone(), Arc::new(opener)).with_refresh(Duration::from_millis(200));
    let first = router.store(r.store).await.unwrap();
    assert!(Arc::ptr_eq(&first, &router.store(r.store).await.unwrap()), "kept while fresh");
    // A second backend is added (and checked) and the first is retired: the router sees it once the entry is old.
    let id = r.repo.add_backend(r.store, "b", "fs", r#"{"dir":"b"}"#, 100).await.unwrap();
    r.repo.record_backend_check(id, "ok", None).await.unwrap();
    r.repo.set_backend_state(r.id_of("a").await, BackendState::ReadOnly).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = router.store(r.store).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second), "opened again");
    let h = second.put(body(1)).await.unwrap();
    assert_eq!(r.holders(&h).await, vec!["b".to_owned()], "the new backend is used and the read-only one is not written");
    // A pool whose backend fell out of its check is refused at the next refresh, with a temporary failure.
    r.repo.record_backend_check(id, "auth", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(router.store(r.store).await, Err(Error::Busy(_))));
}
