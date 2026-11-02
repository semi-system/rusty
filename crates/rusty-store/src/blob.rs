//! Content-addressed blob storage.
//!
//! Garbage-collection support (`list_shard`, `delete_if_older`, `sweep_temp_before`) relies on
//! file mtimes: `put` refreshes the mtime of an existing blob, so a blob that was just put is
//! always younger than GC's grace window, even when `put` took the idempotent early return.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{BlobHash, Error, Result};
use tokio::io::AsyncWriteExt;

/// One stored blob as reported by [`BlobStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobEntry {
    pub hash: BlobHash,
    pub size: u64,
    /// Last modification time, unix seconds (rounded down).
    pub modified_unix: i64,
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `bytes`; idempotent. Storing content that already exists refreshes its
    /// modification time (GC's grace window starts again).
    async fn put(&self, bytes: Bytes) -> Result<BlobHash>;
    async fn get(&self, h: &BlobHash) -> Result<Bytes>;
    async fn delete(&self, h: &BlobHash) -> Result<()>;

    /// Blobs whose hash starts with the two hex digits of `shard` (1/256 of the store), so a
    /// caller can walk the store with memory bounded by one shard. Entries that are not
    /// well-formed blobs (temporary files, odd names, symlinks, directories) and entries that
    /// cannot be read are skipped, never an error.
    async fn list_shard(&self, shard: u8) -> Result<Vec<BlobEntry>>;

    /// Every blob (all 256 shards). Memory grows with the store; prefer `list_shard`.
    async fn list(&self) -> Result<Vec<BlobEntry>> {
        let mut out = Vec::new();
        for shard in 0..=u8::MAX {
            out.extend(self.list_shard(shard).await?);
        }
        Ok(out)
    }

    /// Delete the blob only if it was last modified before `cutoff`. `Ok(true)`: deleted;
    /// `Ok(false)`: missing or too recent (left as is). Must be atomic with respect to a
    /// concurrent `put` of the same content: either the put's mtime refresh is observed (the
    /// blob is kept) or the put finds no blob and writes a fresh copy.
    async fn delete_if_older(&self, h: &BlobHash, cutoff: SystemTime) -> Result<bool>;

    /// Delete leftover temporary files (`.*.tmp`, from writes that never finished) last
    /// modified before `cutoff`; returns how many were deleted.
    async fn sweep_temp_before(&self, cutoff: SystemTime) -> Result<u64>;

    /// `sweep_temp_before(now - older_than)`.
    async fn sweep_temp(&self, older_than: Duration) -> Result<u64> {
        let cutoff = SystemTime::now()
            .checked_sub(older_than)
            .unwrap_or(UNIX_EPOCH);
        self.sweep_temp_before(cutoff).await
    }
}

/// Content-addressed blobs stored at `root/ab/cd/<sha256-hex>`.
///
/// Directory walks never follow symlinks they meet (symlinked shard directories and
/// symlinked blob files are ignored). They use path-based std APIs, so a process that can
/// swap directories under the blob root while GC runs could still race them; the blob root
/// must be writable only by the Rusty service account.
pub struct FsBlobStore {
    root: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FsBlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Only a validated `BlobHash` (64 lowercase hex chars) can build a path.
    fn path_for(&self, h: &BlobHash) -> PathBuf {
        let s = h.as_str();
        self.root.join(&s[0..2]).join(&s[2..4]).join(s)
    }
}

fn io_err(msg: &str, e: io::Error) -> Error {
    Error::storage_with(msg, e)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::storage_with("blob task failed", e))
}

fn tmp_name(h: &BlobHash, tag: &str) -> String {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".{}.{tag}{}.{}.tmp", h.as_str(), std::process::id(), n)
}

/// Mark an existing blob as just used: set its mtime to now, then confirm the path still holds
/// a regular file (GC may have quarantined it meanwhile; then `NotFound` and the caller writes
/// a fresh copy). Anything but a regular file at the path (symlink, directory, FIFO, ...) is
/// an error and is never opened.
fn refresh_mtime(path: &Path) -> io::Result<()> {
    let regular = |p: &Path| -> io::Result<()> {
        if fs::symlink_metadata(p)?.file_type().is_file() {
            Ok(())
        } else {
            Err(io::Error::other("blob path is not a regular file"))
        }
    };
    regular(path)?;
    fs::File::open(path)?.set_modified(SystemTime::now())?;
    regular(path)
}

pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => {
            let d = e.duration();
            let s = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
            (-s).saturating_sub(i64::from(d.subsec_nanos() > 0))
        }
    }
}

/// Two lowercase hex digits (a shard directory name).
fn hex2(name: &OsStr) -> Option<&str> {
    let s = name.to_str()?;
    (s.len() == 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(s)
}

/// A real directory, not a symlink to one.
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}

/// The second-level directories under `root/<l1>`: `(l2 name, path)`. Unreadable or odd
/// entries are skipped.
fn level2_dirs(root: &Path, l1: &str) -> Vec<(String, PathBuf)> {
    let d1 = root.join(l1);
    if !is_real_dir(&d1) {
        return Vec::new();
    }
    let Ok(rd) = fs::read_dir(&d1) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(l2) = hex2(&name) else { continue };
        // `DirEntry::file_type` does not follow symlinks.
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            out.push((l2.to_owned(), e.path()));
        }
    }
    out
}

fn list_shard_sync(root: &Path, shard: u8) -> Vec<BlobEntry> {
    let l1 = format!("{shard:02x}");
    let mut out = Vec::new();
    for (l2, dir) in level2_dirs(root, &l1) {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(s) = name.to_str() else { continue };
            if s.len() != 64 || s[0..2] != l1 || s[2..4] != *l2 {
                continue;
            }
            let Ok(hash) = BlobHash::parse(s) else {
                continue;
            };
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            // On Unix `DirEntry::metadata` does not follow symlinks.
            let Ok(md) = e.metadata() else { continue };
            let Ok(modified) = md.modified() else {
                continue;
            };
            if md.is_file() {
                out.push(BlobEntry {
                    hash,
                    size: md.len(),
                    modified_unix: unix_secs(modified),
                });
            }
        }
    }
    out
}

/// See [`BlobStore::delete_if_older`]. The blob is first renamed to a quarantine name in the
/// same directory, then its mtime is checked on the quarantined file: a concurrent `put` either
/// refreshed the mtime before the rename (the blob is renamed back) or its post-refresh check
/// finds the path empty and it writes a fresh copy. A quarantined file left by a crash matches
/// `.*.tmp` and is removed by a later `sweep_temp`.
fn delete_if_older_sync(path: &Path, h: &BlobHash, cutoff: SystemTime) -> Result<bool> {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err("stat blob", e)),
    };
    if !md.file_type().is_file() {
        return Err(Error::storage("blob path is not a regular file"));
    }
    if md.modified().map_err(|e| io_err("blob mtime", e))? >= cutoff {
        return Ok(false);
    }
    let dir = path
        .parent()
        .ok_or_else(|| Error::storage("blob path has no parent"))?;
    let q = dir.join(tmp_name(h, "gc"));
    match fs::rename(path, &q) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err("quarantine blob", e)),
    }
    let old_file = fs::symlink_metadata(&q)
        .ok()
        .filter(|m| m.file_type().is_file())
        .and_then(|m| m.modified().ok())
        .is_some_and(|m| m < cutoff);
    if !old_file {
        // Refreshed by a concurrent put (or no longer a plain file): put it back. A copy
        // written meanwhile has identical content, so replacing it is harmless.
        fs::rename(&q, path).map_err(|e| io_err("restore blob", e))?;
        return Ok(false);
    }
    match fs::remove_file(&q) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = fs::rename(&q, path);
            Err(io_err("delete blob", e))
        }
    }
}

fn is_tmp_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.len() > ".tmp".len() && s.starts_with('.') && s.ends_with(".tmp"))
}

fn sweep_temp_sync(root: &Path, cutoff: SystemTime) -> u64 {
    let mut removed = 0;
    for shard in 0..=u8::MAX {
        for (_, dir) in level2_dirs(root, &format!("{shard:02x}")) {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                if !is_tmp_name(&e.file_name()) || !e.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let stale = e
                    .metadata()
                    .ok()
                    .filter(|m| m.is_file())
                    .and_then(|m| m.modified().ok())
                    .is_some_and(|m| m < cutoff);
                if stale && fs::remove_file(e.path()).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    removed
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash> {
        let hash = BlobHash::of(&bytes);
        let path = self.path_for(&hash);
        let p = path.clone();
        match blocking(move || refresh_mtime(&p)).await? {
            Ok(()) => return Ok(hash),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("refresh blob", e)),
        }
        let dir = path.parent().ok_or_else(|| Error::storage("blob path has no parent"))?;
        tokio::fs::create_dir_all(dir).await.map_err(|e| io_err("create blob dir", e))?;
        let tmp = dir.join(tmp_name(&hash, ""));

        let write = async {
            let mut f = tokio::fs::File::create(&tmp).await?;
            f.write_all(&bytes).await?;
            f.flush().await?;
            f.sync_all().await?;
            drop(f);
            tokio::fs::rename(&tmp, &path).await
        };
        if let Err(e) = write.await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(io_err("write blob", e));
        }
        Ok(hash)
    }

    async fn get(&self, h: &BlobHash) -> Result<Bytes> {
        match tokio::fs::read(self.path_for(h)).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                Err(Error::NotFound(format!("blob {}", h.as_str())))
            }
            Err(e) => Err(io_err("read blob", e)),
        }
    }

    async fn delete(&self, h: &BlobHash) -> Result<()> {
        match tokio::fs::remove_file(self.path_for(h)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("delete blob", e)),
        }
    }

    async fn list_shard(&self, shard: u8) -> Result<Vec<BlobEntry>> {
        let root = self.root.clone();
        blocking(move || list_shard_sync(&root, shard)).await
    }

    async fn delete_if_older(&self, h: &BlobHash, cutoff: SystemTime) -> Result<bool> {
        let path = self.path_for(h);
        let h = h.clone();
        blocking(move || delete_if_older_sync(&path, &h, cutoff)).await?
    }

    async fn sweep_temp_before(&self, cutoff: SystemTime) -> Result<u64> {
        let root = self.root.clone();
        blocking(move || sweep_temp_sync(&root, cutoff)).await
    }
}
