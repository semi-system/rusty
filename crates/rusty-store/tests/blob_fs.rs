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
