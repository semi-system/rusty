//! Content-addressed blob storage.
//!
//! Garbage-collection support (`list_shard`, `delete_if_older`, `sweep_temp_before`,
//! `heal_quarantine`, `try_lock_gc`) relies on file mtimes: `put` refreshes the mtime of an
//! existing blob, so a blob that was just put is always younger than GC's grace window, even
//! when `put` took the idempotent early return.
//!
//! File names found under the blob root are untrusted: they are validated (`BlobHash::parse`,
//! `str::get`) before any byte-index slicing, and files are opened with
//! `O_NOFOLLOW | O_NONBLOCK` and checked with `fstat` to be regular files.

use std::any::Any;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rustix::fs::{FileType, Mode, OFlags};
use rusty_core::{BlobHash, Error, Result};
use tokio::io::AsyncWriteExt;

/// Suffix of a blob renamed aside by `delete_if_older` (`.<hash>.gc<pid>.<n>.gcq`). Never
/// matched by `sweep_temp`; handled only by `heal_quarantine`.
pub const QUARANTINE_SUFFIX: &str = ".gcq";
/// Name of the advisory GC lock file in the blob root.
pub const GC_LOCK_FILE: &str = ".gc.lock";

/// One stored blob as reported by [`BlobStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobEntry {
    pub hash: BlobHash,
    pub size: u64,
    /// Last modification time, unix seconds (rounded down).
    pub modified_unix: i64,
}

/// Outcome of [`BlobStore::heal_quarantine`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealReport {
    /// Quarantined blobs renamed back because their blob path was missing.
    pub restored: u64,
    /// Quarantine copies removed (blob path present and copy past the cutoff, or a fresh copy
    /// appeared while restoring).
    pub removed: u64,
    /// Quarantine files that could not be handled (left in place).
    pub failed: u64,
}

/// Proof that this process holds the store's GC lock; dropping it releases the lock (also on
/// early return or panic unwinding).
pub struct GcLock {
    _held: Box<dyn Any + Send + Sync>,
}

impl GcLock {
    pub fn new(held: impl Any + Send + Sync) -> GcLock {
        GcLock {
            _held: Box::new(held),
        }
    }
}

impl fmt::Debug for GcLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GcLock")
    }
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
    /// well-formed blobs (temporary or quarantine files, odd names, symlinks, directories)
    /// and entries that cannot be read are skipped, never an error.
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
    /// modified before `cutoff`; returns how many were deleted. Quarantine files are never
    /// touched.
    async fn sweep_temp_before(&self, cutoff: SystemTime) -> Result<u64>;

    /// `sweep_temp_before(now - older_than)`.
    async fn sweep_temp(&self, older_than: Duration) -> Result<u64> {
        let cutoff = SystemTime::now()
            .checked_sub(older_than)
            .unwrap_or(UNIX_EPOCH);
        self.sweep_temp_before(cutoff).await
    }

    /// Repair what an interrupted `delete_if_older` may have left: a quarantined blob whose
    /// blob path is missing is put back (never over an existing file); a quarantine copy
    /// whose blob path exists and that was last modified before `cutoff` is removed; any
    /// other quarantine file is left alone. Call only while holding the GC lock.
    async fn heal_quarantine(&self, cutoff: SystemTime) -> Result<HealReport>;

    /// Take the store's exclusive GC lock without waiting: `Conflict("gc already running")`
    /// if another holder (in this or another process) has it.
    async fn try_lock_gc(&self) -> Result<GcLock>;
}

/// Content-addressed blobs stored at `root/ab/cd/<sha256-hex>`.
///
/// Directory walks never follow symlinks they meet (symlinked shard directories and
/// symlinked blob files are ignored), and blob files are opened with `O_NOFOLLOW |
/// O_NONBLOCK`. Directory traversal itself is path-based, so a process that can swap
/// directories under the blob root while GC runs could still race it; the blob root must be
/// writable only by the Rusty service account.
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

fn next_n() -> u64 {
    TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn tmp_name(h: &BlobHash) -> String {
    format!(".{}.{}.{}.tmp", h.as_str(), std::process::id(), next_n())
}

fn quarantine_name(h: &BlobHash) -> String {
    format!(
        ".{}.gc{}.{}{QUARANTINE_SUFFIX}",
        h.as_str(),
        std::process::id(),
        next_n()
    )
}

/// The blob hash of a quarantine file name `.<hash>.gc<...>.gcq`, if `s` is one.
fn quarantine_hash(s: &str) -> Option<BlobHash> {
    let rest = s.strip_prefix('.')?;
    // `str::get` returns None instead of panicking off a char boundary.
    let hash = rest.get(..64)?;
    let tail = rest.get(64..)?;
    if !tail.starts_with(".gc") || !tail.ends_with(QUARANTINE_SUFFIX) {
        return None;
    }
    BlobHash::parse(hash).ok()
}

fn not_regular() -> io::Error {
    io::Error::other("blob path is not a regular file")
}

/// Open an existing regular file for reading without following a symlink and without
/// blocking on a FIFO; anything but a regular file is an error.
fn open_regular(path: &Path) -> io::Result<fs::File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::empty())?;
    let st = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(not_regular());
    }
    Ok(fs::File::from(fd))
}

/// Mark an existing blob as just used: set its mtime to now, then confirm the path still holds
/// a regular file (GC may have renamed it aside meanwhile; then `NotFound` and the caller
/// writes a fresh copy). Symlinks, directories, FIFOs etc. at the path are errors.
fn refresh_mtime(path: &Path) -> io::Result<()> {
    open_regular(path)?.set_modified(SystemTime::now())?;
    if fs::symlink_metadata(path)?.file_type().is_file() {
        Ok(())
    } else {
        Err(not_regular())
    }
}

fn read_blob(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = open_regular(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
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

/// Two lowercase hex digits (a shard directory name). Checks bytes, no slicing.
fn hex2(name: &OsStr) -> Option<&str> {
    let s = name.to_str()?;
    (s.len() == 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(s)
}

/// A real directory, not a symlink to one.
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}

/// True if the validated hash `h` belongs in directory `l1/l2`.
fn in_dir(h: &BlobHash, l1: &str, l2: &str) -> bool {
    let s = h.as_str(); // 64 ASCII hex chars: byte slicing is safe
    s[0..2] == *l1 && s[2..4] == *l2
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

/// mtime of a directory entry if it is a regular file (never follows a symlink).
fn entry_file_mtime(e: &fs::DirEntry) -> Option<SystemTime> {
    if !e.file_type().is_ok_and(|t| t.is_file()) {
        return None;
    }
    // On Unix `DirEntry::metadata` does not follow symlinks.
    e.metadata()
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok())
}

fn list_shard_sync(root: &Path, shard: u8) -> Vec<BlobEntry> {
    let l1 = format!("{shard:02x}");
    let mut out = Vec::new();
    for (l2, dir) in level2_dirs(root, &l1) {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(s) = name.to_str() else { continue };
            if s.len() != 64 {
                continue;
            }
            let Ok(hash) = BlobHash::parse(s) else {
                continue;
            };
            if !in_dir(&hash, &l1, &l2) {
                continue;
            }
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
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

/// Move quarantine file `q` back to `path`, never replacing a file that exists there (a
/// fresh copy written by a concurrent `put` wins; the old copy is dropped). `Ok(true)`:
/// restored; `Ok(false)`: a file already existed and `q` was removed.
fn restore_no_clobber(q: &Path, path: &Path) -> io::Result<bool> {
    match fs::hard_link(q, path) {
        Ok(()) => {
            // Restored. If removing `q` fails, a later heal removes it once it is old.
            let _ = fs::remove_file(q);
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(q)?;
            Ok(false)
        }
        Err(e) => {
            // No hard links on this filesystem: fall back to rename, but only onto a free path.
            match fs::symlink_metadata(path) {
                Err(m) if m.kind() == ErrorKind::NotFound => fs::rename(q, path).map(|()| true),
                Ok(_) => fs::remove_file(q).map(|()| false),
                Err(_) => Err(e),
            }
        }
    }
}

/// See [`BlobStore::delete_if_older`]. The blob is first renamed to a quarantine name in the
/// same directory, then its mtime is checked on the quarantined file: a concurrent `put` either
/// refreshed the mtime before the rename (the blob is moved back, unless a fresh copy already
/// took its place) or its post-refresh check finds the path empty and it writes a fresh copy.
/// A quarantine file left by a crash is handled by `heal_quarantine` at the next GC start.
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
    let q = dir.join(quarantine_name(h));
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
        // Refreshed by a concurrent put (or no longer a plain file): put it back.
        restore_no_clobber(&q, path).map_err(|e| io_err("restore blob", e))?;
        return Ok(false);
    }
    match fs::remove_file(&q) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = restore_no_clobber(&q, path);
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
                if !is_tmp_name(&e.file_name()) {
                    continue;
                }
                let stale = entry_file_mtime(&e).is_some_and(|m| m < cutoff);
                if stale && fs::remove_file(e.path()).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    removed
}

fn heal_quarantine_sync(root: &Path, cutoff: SystemTime) -> HealReport {
    let mut r = HealReport::default();
    for shard in 0..=u8::MAX {
        let l1 = format!("{shard:02x}");
        for (l2, dir) in level2_dirs(root, &l1) {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name();
                let Some(hash) = name.to_str().and_then(quarantine_hash) else {
                    continue;
                };
                if !in_dir(&hash, &l1, &l2) {
                    continue;
                }
                let Some(q_mtime) = entry_file_mtime(&e) else {
                    continue;
                };
                let q = e.path();
                let path = dir.join(hash.as_str());
                match fs::symlink_metadata(&path) {
                    Err(m) if m.kind() == ErrorKind::NotFound => {
                        match restore_no_clobber(&q, &path) {
                            Ok(true) => r.restored += 1,
                            Ok(false) => r.removed += 1,
                            Err(_) => r.failed += 1,
                        }
                    }
                    Ok(m) if m.file_type().is_file() => {
                        if q_mtime < cutoff {
                            match fs::remove_file(&q) {
                                Ok(()) => r.removed += 1,
                                Err(_) => r.failed += 1,
                            }
                        }
                    }
                    // Something odd squats on the blob path: keep the real content.
                    _ => r.failed += 1,
                }
            }
        }
    }
    r
}

fn lock_gc_sync(root: &Path) -> Result<GcLock> {
    fs::create_dir_all(root).map_err(|e| io_err("create blob root", e))?;
    let flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::open(root.join(GC_LOCK_FILE), flags, Mode::RUSR | Mode::WUSR)
        .map_err(|e| io_err("open gc lock", e.into()))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| io_err("stat gc lock", e.into()))?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(Error::storage("gc lock is not a regular file"));
    }
    let f = fs::File::from(fd);
    match f.try_lock() {
        Ok(()) => Ok(GcLock::new(f)),
        Err(fs::TryLockError::WouldBlock) => Err(Error::Conflict("gc already running".into())),
        Err(fs::TryLockError::Error(e)) => Err(io_err("lock gc", e)),
    }
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
        let tmp = dir.join(tmp_name(&hash));

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

    /// Read a blob. A symlink, FIFO or directory at the blob path is an error (never followed,
    /// never blocks).
    ///
    /// A referenced blob can read `NotFound` only in one narrow race: GC renamed the file
    /// aside (`delete_if_older`) just as a concurrent `put` of the same content refreshed it;
    /// GC then moves it back (or the put writes a fresh copy) within microseconds. Callers do
    /// not retry here.
    async fn get(&self, h: &BlobHash) -> Result<Bytes> {
        let path = self.path_for(h);
        match blocking(move || read_blob(&path)).await? {
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

    async fn heal_quarantine(&self, cutoff: SystemTime) -> Result<HealReport> {
        let root = self.root.clone();
        blocking(move || heal_quarantine_sync(&root, cutoff)).await
    }

    async fn try_lock_gc(&self) -> Result<GcLock> {
        let root = self.root.clone();
        blocking(move || lock_gc_sync(&root)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_no_clobber_restores_into_free_path() {
        let d = tempfile::tempdir().unwrap();
        let (q, p) = (d.path().join(".q.gcq"), d.path().join("blob"));
        fs::write(&q, b"content").unwrap();
        assert!(restore_no_clobber(&q, &p).unwrap());
        assert_eq!(fs::read(&p).unwrap(), b"content");
        assert!(!q.exists());
    }

    #[test]
    fn restore_no_clobber_never_replaces_a_fresh_copy() {
        let d = tempfile::tempdir().unwrap();
        let (q, p) = (d.path().join(".q.gcq"), d.path().join("blob"));
        fs::write(&q, b"old inode").unwrap();
        fs::write(&p, b"fresh copy").unwrap();
        assert!(!restore_no_clobber(&q, &p).unwrap());
        assert_eq!(fs::read(&p).unwrap(), b"fresh copy");
        assert!(!q.exists());
    }

    #[test]
    fn quarantine_names_round_trip_and_reject_odd_names() {
        let h = BlobHash::of(b"x");
        let n = quarantine_name(&h);
        assert!(n.ends_with(QUARANTINE_SUFFIX));
        assert_eq!(quarantine_hash(&n), Some(h.clone()));
        assert!(!is_tmp_name(OsStr::new(&n)));
        for odd in [
            String::new(),
            ".".into(),
            format!(".€{}.gc1.1.gcq", "0".repeat(61)),
            format!(".{}€.gc1.1.gcq", "0".repeat(62)),
            format!(".{}.gc1.1.tmp", h.as_str()),
            format!(".{}.xx1.1.gcq", h.as_str()),
            format!("{}.gc1.1.gcq", h.as_str()),
        ] {
            assert_eq!(quarantine_hash(&odd), None, "{odd:?}");
        }
    }
}
