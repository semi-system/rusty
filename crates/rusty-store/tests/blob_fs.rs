use std::sync::Arc;

use bytes::Bytes;
use rusty_core::{BlobHash, Error};
use rusty_store::{BlobStore, FsBlobStore};

fn store() -> (tempfile::TempDir, FsBlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = FsBlobStore::new(dir.path().to_path_buf());
    (dir, s)
}

fn all_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[tokio::test]
async fn put_is_idempotent_and_content_addressed() {
    let (dir, s) = store();
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

#[tokio::test]
async fn get_missing_is_not_found() {
    let (_d, s) = store();
    let h = BlobHash::of(b"nope");
    assert!(matches!(s.get(&h).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn delete_missing_is_ok() {
    let (_d, s) = store();
    s.delete(&BlobHash::of(b"nope")).await.unwrap();
}

#[tokio::test]
async fn delete_removes_file_then_get_not_found() {
    let (_d, s) = store();
    let h = s.put(Bytes::from_static(b"x")).await.unwrap();
    s.delete(&h).await.unwrap();
    assert!(matches!(s.get(&h).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn round_trip_empty_and_large() {
    let (_d, s) = store();
    for data in [Bytes::new(), Bytes::from(vec![7u8; 1024 * 1024])] {
        let h = s.put(data.clone()).await.unwrap();
        assert_eq!(s.get(&h).await.unwrap(), data);
    }
}

#[tokio::test]
async fn put_writes_via_temp_file_then_rename() {
    let (dir, s) = store();
    s.put(Bytes::from_static(b"abc")).await.unwrap();
    let files = all_files(dir.path());
    assert_eq!(files.len(), 1);
    assert!(files.iter().all(|p| !p.to_string_lossy().ends_with(".tmp")));
}

#[tokio::test]
async fn concurrent_puts_of_same_content_succeed() {
    let (dir, s) = store();
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

#[tokio::test]
async fn put_refreshes_mtime_of_existing_blob() {
    let (dir, s) = store();
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

#[tokio::test]
async fn list_returns_only_well_formed_blobs() {
    let (dir, s) = store();
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

#[tokio::test]
async fn list_of_missing_root_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let s = FsBlobStore::new(dir.path().join("does-not-exist"));
    assert!(s.list().await.unwrap().is_empty());
    assert_eq!(s.sweep_temp(Duration::ZERO).await.unwrap(), 0);
}

#[tokio::test]
async fn delete_if_older_respects_cutoff() {
    let (dir, s) = store();
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

#[tokio::test]
async fn sweep_temp_removes_only_stale_tmp_files() {
    let (dir, s) = store();
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
