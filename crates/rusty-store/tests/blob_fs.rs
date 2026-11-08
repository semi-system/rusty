use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use bytes::Bytes;
use rusty_core::{BlobHash, Error};
use rusty_store::{BlobStore, FsBlobStore, BLOB_ROOT_MARKER, GC_LOCK_FILE};

/// A fresh, initialized and opened blob store whose root is the temp dir itself.
async fn store() -> (tempfile::TempDir, FsBlobStore) {
    let dir = tempfile::tempdir().unwrap();
    FsBlobStore::init(dir.path()).await.unwrap();
    let s = FsBlobStore::open(dir.path()).await.unwrap();
    (dir, s)
}

/// Files under `root`, except the marker and the GC lock file in the root itself.
fn all_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if d == root && (p.ends_with(BLOB_ROOT_MARKER) || p.ends_with(GC_LOCK_FILE)) {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn put_is_idempotent_and_content_addressed() {
    let (dir, s) = store().await;
    let data = Bytes::from_static(b"hello blob");
    let h1 = s.put(data.clone()).await.unwrap();
    let h2 = s.put(data.clone()).await.unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1, BlobHash::of(b"hello blob"));
    let hs = h1.as_str();
    let path = dir.path().join(&hs[0..2]).join(&hs[2..4]).join(hs);
    assert!(path.is_file());
    assert_eq!(s.get(&h1).await.unwrap(), data);
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn get_missing_is_not_found() {
    let (_d, s) = store().await;
    let h = BlobHash::of(b"nope");
    assert!(matches!(s.get(&h).await, Err(Error::NotFound(_))));
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn delete_if_older_then_get_not_found() {
    let (_d, s) = store().await;
    let h = s.put(Bytes::from_static(b"x")).await.unwrap();
    let future = SystemTime::now() + DAY;
    assert!(s.delete_if_older(&h, future).await.unwrap());
    assert!(matches!(s.get(&h).await, Err(Error::NotFound(_))));
    assert!(!s
        .delete_if_older(&BlobHash::of(b"nope"), future)
        .await
        .unwrap());
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn round_trip_empty_and_large() {
    let (_d, s) = store().await;
    for data in [Bytes::new(), Bytes::from(vec![7u8; 1024 * 1024])] {
        let h = s.put(data.clone()).await.unwrap();
        assert_eq!(s.get(&h).await.unwrap(), data);
    }
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn put_writes_via_temp_file_then_rename() {
    let (dir, s) = store().await;
    s.put(Bytes::from_static(b"abc")).await.unwrap();
    let files = all_files(dir.path());
    assert_eq!(files.len(), 1);
    assert!(files.iter().all(|p| !p.to_string_lossy().ends_with(".tmp")));
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn concurrent_puts_of_same_content_succeed() {
    let (dir, s) = store().await;
    let s = Arc::new(s);
    let data = Bytes::from(vec![42u8; 256 * 1024]);
    let mut tasks = vec![];
    for _ in 0..16 {
        let (s, d) = (s.clone(), data.clone());
        tasks.push(tokio::spawn(async move { s.put(d).await }));
    }
    let expected = BlobHash::of(&data);
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap(), expected);
    }
    let files = all_files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(s.get(&expected).await.unwrap(), data);
}

// ----- list / delete_if_older / sweep_temp / mtime refresh (GC support) -----

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAY: Duration = Duration::from_secs(86_400);

fn blob_path(root: &Path, h: &BlobHash) -> PathBuf {
    let s = h.as_str();
    root.join(&s[0..2]).join(&s[2..4]).join(s)
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::symlink_metadata(p).unwrap().modified().unwrap()
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn put_refreshes_mtime_of_existing_blob() {
    let (dir, s) = store().await;
    let h = s.put(Bytes::from_static(b"again")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    let old = SystemTime::now() - 3 * DAY;
    set_mtime(&p, old);
    assert!(mtime(&p) < SystemTime::now() - 2 * DAY);
    let before = SystemTime::now() - Duration::from_secs(1);
    assert_eq!(s.put(Bytes::from_static(b"again")).await.unwrap(), h);
    assert!(mtime(&p) >= before, "idempotent put must refresh the mtime");
    assert_eq!(s.get(&h).await.unwrap(), Bytes::from_static(b"again"));
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn list_returns_only_well_formed_blobs() {
    let (dir, s) = store().await;
    let h1 = s.put(Bytes::from_static(b"one")).await.unwrap();
    let h2 = s.put(Bytes::from_static(b"second blob")).await.unwrap();
    let t = SystemTime::now() - DAY;
    set_mtime(&blob_path(dir.path(), &h1), t);
    // noise that must be ignored
    let d = blob_path(dir.path(), &h1).parent().unwrap().to_path_buf();
    std::fs::write(d.join(format!(".{}.1.2.tmp", h1.as_str())), b"partial").unwrap();
    std::fs::write(d.join("README"), b"x").unwrap();
    std::fs::write(dir.path().join("README"), b"x").unwrap();

    let mut got = s.list().await.unwrap();
    got.sort_by(|a, b| a.hash.cmp(&b.hash));
    let mut want = vec![(h1.clone(), 3u64), (h2.clone(), 11u64)];
    want.sort();
    assert_eq!(
        got.iter()
            .map(|e| (e.hash.clone(), e.size))
            .collect::<Vec<_>>(),
        want
    );
    let e1 = got.iter().find(|e| e.hash == h1).unwrap();
    assert_eq!(e1.modified_unix, unix(t));

    let shard = u8::from_str_radix(&h2.as_str()[0..2], 16).unwrap();
    let in_shard = s.list_shard(shard).await.unwrap();
    assert!(in_shard.iter().any(|e| e.hash == h2));
    assert!(in_shard
        .iter()
        .all(|e| e.hash.as_str().starts_with(&h2.as_str()[0..2])));
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn store_with_no_blobs_lists_nothing() {
    let (_d, s) = store().await;
    assert!(s.list().await.unwrap().is_empty());
    assert_eq!(s.sweep_temp(Duration::ZERO).await.unwrap(), 0);
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn delete_if_older_respects_cutoff() {
    let (dir, s) = store().await;
    let h = s.put(Bytes::from_static(b"aging")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    let now = SystemTime::now();
    set_mtime(&p, now - DAY);

    // file newer than the cutoff: kept, content intact, no quarantine leftovers
    assert!(!s.delete_if_older(&h, now - 2 * DAY).await.unwrap());
    assert_eq!(s.get(&h).await.unwrap(), Bytes::from_static(b"aging"));
    assert_eq!(all_files(dir.path()), vec![p.clone()]);
    assert_eq!(mtime(&p), now - DAY, "a kept file keeps its mtime");

    // older than the cutoff: deleted
    assert!(s
        .delete_if_older(&h, now - Duration::from_secs(3600))
        .await
        .unwrap());
    assert!(!p.exists());
    assert!(all_files(dir.path()).is_empty());

    // missing: nothing to do
    assert!(!s.delete_if_older(&h, now).await.unwrap());

    // a directory squatting on a blob path is an error, never removed
    std::fs::create_dir_all(&p).unwrap();
    assert!(s.delete_if_older(&h, now + DAY).await.is_err());
    assert!(p.is_dir());
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn sweep_temp_removes_only_stale_tmp_files() {
    let (dir, s) = store().await;
    let h = s.put(Bytes::from_static(b"keep me")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    let d = p.parent().unwrap().to_path_buf();
    let old = SystemTime::now() - DAY;
    set_mtime(&p, old);

    let stale = d.join(format!(".{}.77.3.tmp", h.as_str()));
    let fresh = d.join(format!(".{}.77.4.tmp", h.as_str()));
    let no_dot = d.join("x.tmp");
    let root_tmp = dir.path().join(".r.tmp");
    let level1_tmp = d.parent().unwrap().join(".l.tmp");
    for f in [&stale, &fresh, &no_dot, &root_tmp, &level1_tmp] {
        std::fs::write(f, b"partial").unwrap();
    }
    for f in [&stale, &no_dot, &root_tmp, &level1_tmp] {
        set_mtime(f, old);
    }
    let tmp_dir = d.join(".dir.tmp");
    std::fs::create_dir(&tmp_dir).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target");
    std::fs::write(&target, b"outside").unwrap();
    set_mtime(&target, old);
    let link = d.join(".link.tmp");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    assert_eq!(s.sweep_temp(Duration::from_secs(3600)).await.unwrap(), 1);
    assert!(!stale.exists());
    for f in [&fresh, &no_dot, &root_tmp, &level1_tmp, &p, &target] {
        assert!(f.exists(), "{f:?} must survive");
    }
    assert!(tmp_dir.is_dir());
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn sweep_temp_ignores_quarantine_files() {
    let (dir, s) = store().await;
    let h = s.put(Bytes::from_static(b"quarantined")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    let q = p.with_file_name(format!(
        ".{}.gc4242.7{}",
        h.as_str(),
        rusty_store::QUARANTINE_SUFFIX
    ));
    std::fs::rename(&p, &q).unwrap();
    set_mtime(&q, SystemTime::now() - 30 * DAY);

    assert_eq!(s.sweep_temp(Duration::ZERO).await.unwrap(), 0);
    assert_eq!(
        s.sweep_temp_before(SystemTime::now() + DAY).await.unwrap(),
        0
    );
    assert!(q.is_file(), "a quarantine copy is never swept");
    assert!(s.list().await.unwrap().is_empty(), "nor listed as a blob");
}

/// M3: a symlink or FIFO squatting on a blob path is never followed and never blocks.
// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn symlink_or_fifo_at_blob_path_is_not_followed_and_does_not_block() {
    let (dir, s) = store().await;
    let limit = Duration::from_secs(5);

    // FIFO at the path of `fifo_data`
    let fifo_data = Bytes::from_static(b"fifo content");
    let hf = BlobHash::of(&fifo_data);
    let pf = blob_path(dir.path(), &hf);
    std::fs::create_dir_all(pf.parent().unwrap()).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &pf,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let got = tokio::time::timeout(limit, s.get(&hf))
        .await
        .expect("get must not block");
    assert!(matches!(got, Err(Error::Storage { .. })), "{got:?}");
    let put = tokio::time::timeout(limit, s.put(fifo_data.clone()))
        .await
        .expect("put must not block");
    assert!(put.is_err(), "put over a FIFO fails closed");
    let del = tokio::time::timeout(limit, s.delete_if_older(&hf, SystemTime::now() + DAY))
        .await
        .expect("delete_if_older must not block");
    assert!(del.is_err());

    // symlink at the path of `link_data`, pointing at an outside file
    let link_data = Bytes::from_static(b"link content");
    let hl = BlobHash::of(&link_data);
    let pl = blob_path(dir.path(), &hl);
    std::fs::create_dir_all(pl.parent().unwrap()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    std::fs::write(&secret, b"outside secret").unwrap();
    let old = SystemTime::now() - DAY;
    set_mtime(&secret, old);
    std::os::unix::fs::symlink(&secret, &pl).unwrap();
    let got = s.get(&hl).await;
    assert!(
        matches!(got, Err(Error::Storage { .. })),
        "never follows: {got:?}"
    );
    assert!(s.put(link_data.clone()).await.is_err());
    assert_eq!(mtime(&secret), old, "the symlink target is not touched");
    assert_eq!(std::fs::read(&secret).unwrap(), b"outside secret");

    assert!(s.list().await.unwrap().is_empty());
    assert!(
        pf.exists()
            && std::fs::symlink_metadata(&pl)
                .unwrap()
                .file_type()
                .is_symlink()
    );
}

// ----- B2: explicit init, fail-closed open -----

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn init_creates_root_0700_with_marker_0600_and_is_idempotent() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("blobs");
    FsBlobStore::init(&root).await.unwrap();
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join(BLOB_ROOT_MARKER)), 0o600);
    FsBlobStore::init(&root).await.unwrap();
    FsBlobStore::open(&root).await.unwrap();
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn open_fails_closed_on_missing_root_and_never_creates_it() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("blobs");
    assert!(FsBlobStore::open(&root).await.is_err());
    assert!(!root.exists(), "open must not create the root");
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn open_requires_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    assert!(FsBlobStore::open(dir.path()).await.is_err());
    // A directory named like the marker is not a marker.
    std::fs::create_dir(dir.path().join(BLOB_ROOT_MARKER)).unwrap();
    assert!(FsBlobStore::open(dir.path()).await.is_err());
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn open_rejects_a_symlinked_root() {
    let parent = tempfile::tempdir().unwrap();
    let real = parent.path().join("real");
    FsBlobStore::init(&real).await.unwrap();
    let link = parent.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(FsBlobStore::open(&link).await.is_err());
    assert!(FsBlobStore::init(&link).await.is_err());
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn open_rejects_a_group_or_world_writable_root() {
    let (dir, _s) = store().await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(FsBlobStore::open(dir.path()).await.is_err());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    FsBlobStore::open(dir.path()).await.unwrap();
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn init_refuses_a_non_empty_directory_without_marker() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("something"), b"x").unwrap();
    assert!(FsBlobStore::init(dir.path()).await.is_err());
    assert!(!dir.path().join(BLOB_ROOT_MARKER).exists());
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn put_and_gc_lock_never_create_a_vanished_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("blobs");
    FsBlobStore::init(&root).await.unwrap();
    let s = FsBlobStore::open(&root).await.unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(s.put(Bytes::from_static(b"lost")).await.is_err());
    assert!(s.try_lock_gc().await.is_err());
    assert!(!root.exists(), "the root must not be recreated");
}

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn open_heals_quarantine_when_gc_lock_is_free() {
    let (dir, s) = store().await;
    let h = s.put(Bytes::from_static(b"crashed gc")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    let q = p.with_file_name(format!(
        ".{}.gc1.1{}",
        h.as_str(),
        rusty_store::QUARANTINE_SUFFIX
    ));
    std::fs::rename(&p, &q).unwrap();

    // GC lock held elsewhere: open still succeeds and leaves the quarantine file alone.
    let lock = s.try_lock_gc().await.unwrap();
    FsBlobStore::open(dir.path()).await.unwrap();
    assert!(q.exists() && !p.exists());
    drop(lock);

    // Lock free: open restores the blob.
    let s2 = FsBlobStore::open(dir.path()).await.unwrap();
    assert!(p.is_file() && !q.exists());
    assert_eq!(s2.get(&h).await.unwrap(), Bytes::from_static(b"crashed gc"));
}

// ----- B3: file modes -----

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn blob_files_are_0600_and_shard_dirs_0700() {
    let (dir, s) = store().await;
    let h = s.put(Bytes::from_static(b"private")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    assert_eq!(mode(&p), 0o600);
    assert_eq!(mode(p.parent().unwrap()), 0o700);
    assert_eq!(mode(p.parent().unwrap().parent().unwrap()), 0o700);
    let _lock = s.try_lock_gc().await.unwrap();
    assert_eq!(mode(&dir.path().join(GC_LOCK_FILE)), 0o600);
}

// ----- B5: put racing delete_if_older -----

/// `put(h)` runs concurrently with `delete_if_older(h, cutoff)` while the stored copy is older
/// than the cutoff. Invariant: once both finished, `get(h)` succeeds whenever `put` returned
/// `Ok` (put never reports success for a blob that GC then loses).
///
/// Each operation runs on its own OS thread (released together by a barrier, then delayed by
/// a pseudo-random spin from a fixed seed), so neither can starve the other on a shared
/// runtime worker. Both orders and the overlapping interleavings all occur.
// backend-neutral: filesystem blob store only, no database
#[test]
fn put_racing_delete_if_older_never_loses_the_blob() {
    const ITERATIONS: usize = 2000;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    let (dir, s) = handle.block_on(store());
    let contents: Vec<Bytes> = (0..4)
        .map(|i| Bytes::from(format!("race content {i}")))
        .collect();
    let started = std::time::Instant::now();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15; // deterministic xorshift64
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut deleted, mut kept) = (0u32, 0u32);
    for i in 0..ITERATIONS {
        let data = contents[i % contents.len()].clone();
        let h = BlobHash::of(&data);
        let p = blob_path(dir.path(), &h);
        if !p.exists() {
            handle.block_on(s.put(data.clone())).unwrap();
        }
        let now = SystemTime::now();
        set_mtime(&p, now - DAY);
        let cutoff = now - Duration::from_secs(3600);
        // One operation takes ~100 us; a spin step is tens of ns. Spread both over ~0..200 us.
        let (spin_put, spin_del) = (next() % 5_000, next() % 5_000);
        let barrier = std::sync::Barrier::new(2);
        let (put, del) = std::thread::scope(|sc| {
            let put = sc.spawn(|| {
                barrier.wait();
                for _ in 0..spin_put {
                    std::hint::spin_loop();
                }
                handle.block_on(s.put(data.clone()))
            });
            let del = sc.spawn(|| {
                barrier.wait();
                for _ in 0..spin_del {
                    std::hint::spin_loop();
                }
                handle.block_on(s.delete_if_older(&h, cutoff))
            });
            (put.join().unwrap(), del.join().unwrap())
        });
        let del = del.unwrap();
        if del {
            deleted += 1;
        } else {
            kept += 1;
        }
        assert_eq!(put.unwrap(), h, "iteration {i}: put failed");
        let got = handle.block_on(s.get(&h));
        assert!(
            matches!(&got, Ok(b) if *b == data),
            "iteration {i}: put returned Ok but get gave {got:?} (deleted={del})"
        );
    }
    let elapsed = started.elapsed();
    eprintln!("race: {ITERATIONS} iterations, delete won {deleted}, kept {kept}, {elapsed:?}");
    assert!(elapsed < Duration::from_secs(10), "too slow: {elapsed:?}");
    assert!(deleted > 0 && kept > 0, "both outcomes must occur");
    assert!(all_files(dir.path()).iter().all(|f| {
        let n = f.file_name().unwrap().to_string_lossy().into_owned();
        !n.ends_with(".tmp") && !n.ends_with(rusty_store::QUARANTINE_SUFFIX)
    }));
}

// ----- P0 m1: symlinked roots are refused, with or without a trailing slash -----

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn open_and_init_refuse_symlink_root_even_with_trailing_slash() {
    let parent = tempfile::tempdir().unwrap();
    let real = parent.path().join("real");
    FsBlobStore::init(&real).await.unwrap();
    let link = parent.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let with_slash = format!("{}/", link.display());
    assert!(FsBlobStore::open(&with_slash).await.is_err());
    assert!(FsBlobStore::open(&link).await.is_err());
    assert!(FsBlobStore::init(&with_slash).await.is_err());
    assert!(FsBlobStore::init(&link).await.is_err());
    // The real root still opens, with or without a trailing slash.
    FsBlobStore::open(&real).await.unwrap();
    FsBlobStore::open(format!("{}/", real.display()))
        .await
        .unwrap();
}

// ----- P0 m5: the startup heal report is kept -----

// backend-neutral: filesystem blob store only, no database
#[tokio::test]
async fn startup_heal_report_is_available() {
    let (dir, s) = store().await;
    assert_eq!(s.startup_heal().map(|r| r.restored), Some(0));
    let h = s.put(Bytes::from_static(b"crashed gc")).await.unwrap();
    let p = blob_path(dir.path(), &h);
    let q = p.with_file_name(format!(
        ".{}.gc1.1{}",
        h.as_str(),
        rusty_store::QUARANTINE_SUFFIX
    ));
    std::fs::rename(&p, &q).unwrap();

    let lock = s.try_lock_gc().await.unwrap();
    let skipped = FsBlobStore::open(dir.path()).await.unwrap();
    assert!(skipped.startup_heal().is_none());
    drop(lock);

    let healed = FsBlobStore::open(dir.path()).await.unwrap();
    let r = healed.startup_heal().expect("heal ran");
    assert_eq!(r.restored, 1);
}
