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
//!
//! Blob root: created only by the explicit [`FsBlobStore::init`] (mode 0700, marker file
//! [`BLOB_ROOT_MARKER`] 0600). The root path is rebuilt from its components first, so a
//! trailing slash cannot make the kernel follow a symlink root. [`FsBlobStore::open`] fails closed when the root is missing,
//! is not a real directory, is writable by group or others, or lacks the marker; nothing
//! else (put, GC, the lock) ever creates the root, so a missing mount or a typo in the
//! configuration cannot silently start an empty store.
//!
//! Permissions: shard directories are created 0700, temporary and blob files 0600 with
//! `O_CREAT | O_EXCL | O_NOFOLLOW` (a pre-planted file or symlink at a temporary name is never
//! opened; a new random name is tried instead).
//!
//! Durability: `put` fsyncs the file, renames it into place and then fsyncs the shard
//! directory, its parent and the blob root, so a blob whose `put` returned `Ok` survives a
//! crash (fsync of an unchanged directory is cheap). A `put` of content that already exists
//! runs the same three directory fsyncs before returning `Ok`. Restoring a quarantined blob
//! (`restore_no_clobber`, used by `delete_if_older` and `heal_quarantine`) fsyncs the
//! directory. The rename-aside and the unlink in `delete_if_older` are not fsynced: if a crash
//! undoes either, the blob or its quarantine copy reappears, which only resurrects garbage
//! that a later GC run removes again (never loses a referenced blob).

use std::any::Any;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rand_core::{OsRng, RngCore};
use rustix::fs::{FileType, Mode, OFlags};
use rusty_core::{BlobHash, Error, Result};

/// Suffix of a blob renamed aside by `delete_if_older` (`.<hash>.gc<pid>.<n>.gcq`). Never
/// matched by `sweep_temp`; handled only by `heal_quarantine`.
pub const QUARANTINE_SUFFIX: &str = ".gcq";
/// Name of the advisory GC lock file in the blob root.
pub const GC_LOCK_FILE: &str = ".gc.lock";
/// Name of the marker file written by [`FsBlobStore::init`]; [`FsBlobStore::open`] requires it.
pub const BLOB_ROOT_MARKER: &str = ".rusty-blobs";
const MARKER_CONTENT: &[u8] = b"rusty blob store v1\n";
/// Temporary-name collisions tolerated by `put` before it gives up.
const TMP_ATTEMPTS: usize = 8;

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
    pub(crate) fn new(held: impl Any + Send + Sync) -> GcLock {
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

    /// The clock that stamps this store's file modification times: the operating system clock of
    /// the host that owns the volume (`put` refreshes mtimes with it). GC compares it with the
    /// database clock before it judges any file (clock policy, rule 4).
    fn fs_now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Content-addressed blobs stored at `root/ab/cd/<sha256-hex>`.
///
/// Directory walks never follow symlinks they meet (symlinked shard directories and
/// symlinked blob files are ignored), and blob files are opened with `O_NOFOLLOW |
/// O_NONBLOCK`. Directory traversal itself is path-based, so a process that can swap
/// directories under the blob root while GC runs could still race it; the blob root must be
/// writable only by the Rusty service account.
///
/// Built only by [`FsBlobStore::open`] on a root prepared once by [`FsBlobStore::init`].
pub struct FsBlobStore {
    root: PathBuf,
    startup_heal: Option<HealReport>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FsBlobStore {
    /// Prepare a blob root (an explicit administrative step, never implicit): create `root`
    /// with mode 0700 if it is absent (its parent must exist), then write the marker
    /// [`BLOB_ROOT_MARKER`] (0600). Idempotent on an initialized root. A symlink, a
    /// non-directory, or a non-empty directory without the marker is refused.
    pub async fn init(root: impl AsRef<Path>) -> Result<()> {
        let root = normalize_root(root.as_ref());
        blocking(move || init_sync(&root)).await?
    }

    /// Open an initialized blob root. Fails closed (`Storage`) if `root` does not exist, is
    /// not a real directory (symlinks are refused), is writable by group or others, or has no
    /// marker file; it never creates anything. If the GC lock is free, quarantine files left
    /// by an interrupted GC are healed once (see [`BlobStore::heal_quarantine`], cutoff
    /// `now - DEFAULT_GC_GRACE`); if another process holds the lock this step is skipped.
    /// The heal scans up to 65,536 shard directories (256 x 256), so `open` on a large store
    /// takes proportionally long; see [`FsBlobStore::startup_heal`] for its report.
    pub async fn open(root: impl AsRef<Path>) -> Result<FsBlobStore> {
        let root = normalize_root(root.as_ref());
        let r = root.clone();
        blocking(move || check_root(&r)).await??;
        let mut store = FsBlobStore {
            root,
            startup_heal: None,
        };
        match store.try_lock_gc().await {
            Ok(lock) => {
                let cutoff = SystemTime::now()
                    .checked_sub(crate::gc::DEFAULT_GC_GRACE)
                    .unwrap_or(UNIX_EPOCH);
                store.startup_heal = Some(store.heal_quarantine(cutoff).await?);
                drop(lock);
            }
            Err(Error::Conflict(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(store)
    }

    /// The report of the quarantine heal that [`FsBlobStore::open`] ran, or `None` if it was
    /// skipped because another process held the GC lock.
    pub fn startup_heal(&self) -> Option<HealReport> {
        self.startup_heal
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

/// `.<hash>.<pid>.<n>.<16 random hex digits>.tmp`: unpredictable, so a hostile local process
/// cannot pre-plant a file or symlink at the name `put` will use.
fn tmp_name(h: &BlobHash) -> io::Result<String> {
    let mut b = [0u8; 8];
    OsRng
        .try_fill_bytes(&mut b)
        .map_err(|_| io::Error::other("no randomness for a temporary name"))?;
    Ok(format!(
        ".{}.{}.{}.{:016x}.tmp",
        h.as_str(),
        std::process::id(),
        next_n(),
        u64::from_le_bytes(b)
    ))
}

/// `O_NOFOLLOW | O_CLOEXEC` as raw open flags for `OpenOptionsExt::custom_flags`.
fn nofollow_cloexec() -> i32 {
    (OFlags::NOFOLLOW | OFlags::CLOEXEC).bits() as i32
}

/// Create a new private (0600) temporary file in `dir` with `O_CREAT | O_EXCL | O_NOFOLLOW`.
/// A name that already exists (a leftover, or a planted file or symlink) is never opened;
/// another name from `name` is tried, at most [`TMP_ATTEMPTS`] times.
fn create_temp(
    dir: &Path,
    mut name: impl FnMut() -> io::Result<String>,
) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..TMP_ATTEMPTS {
        let p = dir.join(name()?);
        let res = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nofollow_cloexec())
            .open(&p);
        match res {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free temporary file name"))
}

/// Directories passed to [`fsync_dir`], recorded for tests.
#[cfg(test)]
static DIR_FSYNCS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Rebuild `root` from its components: drops a trailing slash and `.` segments, so the final
/// path component is the one `lstat` sees (a symlink given as `link/` would otherwise be
/// followed by the kernel).
fn normalize_root(root: &Path) -> PathBuf {
    root.components().collect()
}

/// fsync a directory (makes renames, links and new entries in it durable).
fn fsync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Ok(mut v) = DIR_FSYNCS.lock() {
        v.push(dir.to_path_buf());
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::open(dir, flags, Mode::empty())?;
    rustix::fs::fsync(&fd)?;
    Ok(())
}

/// Create the shard directory `p` with mode 0700 unless it exists (its parent must exist:
/// the blob root is never created here). An existing non-directory or symlink is an error.
fn ensure_shard_dir(p: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists && is_real_dir(p) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(io::Error::other("blob shard path is not a directory"))
        }
        Err(e) => Err(e),
    }
}

/// fsync the shard directory of the blob at `path`, its parent and the blob root. Also run on
/// the idempotent early return of `put`: the earlier put that created the blob may have
/// crashed before its own directory syncs, so a repeated `put` re-establishes durability.
fn sync_shard_chain(root: &Path, path: &Path) -> io::Result<()> {
    let l2 = path
        .parent()
        .ok_or_else(|| io::Error::other("blob path has no parent"))?;
    let l1 = l2
        .parent()
        .ok_or_else(|| io::Error::other("blob path has no parent"))?;
    fsync_dir(l2)?;
    fsync_dir(l1)?;
    fsync_dir(root)
}

/// Write `bytes` as the blob at `root/l1/l2/<hash>` = `path` (see the module docs for the
/// permission and durability protocol).
fn write_blob_sync(root: &Path, path: &Path, h: &BlobHash, bytes: &[u8]) -> io::Result<()> {
    let l2 = path
        .parent()
        .ok_or_else(|| io::Error::other("blob path has no parent"))?;
    let l1 = l2
        .parent()
        .ok_or_else(|| io::Error::other("blob path has no parent"))?;
    ensure_shard_dir(l1)?;
    ensure_shard_dir(l2)?;
    let (tmp, mut f) = create_temp(l2, || tmp_name(h))?;
    let res = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        // The new entry, and (when a concurrent or earlier put created them) the shard
        // directories, must reach the disk before `put` reports success.
        sync_shard_chain(root, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// `open` precondition: `root` is a real directory, not writable by group or others, with a
/// regular marker file.
fn check_root(root: &Path) -> Result<()> {
    let md = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(Error::storage("blob root does not exist (run init first)"));
        }
        Err(e) => return Err(io_err("stat blob root", e)),
    };
    if !md.file_type().is_dir() {
        return Err(Error::storage("blob root is not a directory"));
    }
    if md.permissions().mode() & 0o022 != 0 {
        return Err(Error::storage("blob root is writable by group or others"));
    }
    match fs::symlink_metadata(root.join(BLOB_ROOT_MARKER)) {
        Ok(m) if m.file_type().is_file() => Ok(()),
        Ok(_) => Err(Error::storage("blob root marker is not a regular file")),
        Err(e) if e.kind() == ErrorKind::NotFound => Err(Error::storage(
            "blob root is not initialized (marker missing)",
        )),
        Err(e) => Err(io_err("stat blob root marker", e)),
    }
}

/// See [`FsBlobStore::init`].
fn init_sync(root: &Path) -> Result<()> {
    let created = match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => true,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => false,
        Err(e) => return Err(io_err("create blob root", e)),
    };
    if !is_real_dir(root) {
        return Err(Error::storage("blob root is not a directory"));
    }
    let marker = root.join(BLOB_ROOT_MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(m) if m.file_type().is_file() => return Ok(()),
        Ok(_) => return Err(Error::storage("blob root marker is not a regular file")),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("stat blob root marker", e)),
    }
    let mut entries = fs::read_dir(root).map_err(|e| io_err("read blob root", e))?;
    if entries.next().is_some() {
        return Err(Error::Invalid(
            "blob root is not empty and has no marker".into(),
        ));
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow_cloexec())
        .open(&marker)
        .map_err(|e| io_err("create blob root marker", e))?;
    f.write_all(MARKER_CONTENT)
        .and_then(|()| f.sync_all())
        .map_err(|e| io_err("write blob root marker", e))?;
    fsync_dir(root).map_err(|e| io_err("sync blob root", e))?;
    if created {
        if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsync_dir(parent).map_err(|e| io_err("sync blob root parent", e))?;
        }
    }
    Ok(())
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
/// restored; `Ok(false)`: a file already existed and `q` was removed. A restore is made
/// durable (directory fsync) before `q` is removed.
fn restore_no_clobber(q: &Path, path: &Path) -> io::Result<bool> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("blob path has no parent"))?;
    match fs::hard_link(q, path) {
        Ok(()) => {
            fsync_dir(dir)?;
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
                Err(m) if m.kind() == ErrorKind::NotFound => {
                    fs::rename(q, path)?;
                    fsync_dir(dir)?;
                    Ok(true)
                }
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

/// Take the advisory GC lock. Never creates the blob root: a missing root is an error.
fn lock_gc_sync(root: &Path) -> Result<GcLock> {
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
        let root = self.root.clone();
        match blocking(move || refresh_mtime(&p).and_then(|()| sync_shard_chain(&root, &p))).await?
        {
            Ok(()) => return Ok(hash),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("refresh blob", e)),
        }
        // Runs to completion on a blocking thread even if this future is dropped, so a
        // cancelled put never leaves a half-written temporary file behind.
        let (root, h) = (self.root.clone(), hash.clone());
        blocking(move || write_blob_sync(&root, &path, &h, &bytes))
            .await?
            .map_err(|e| io_err("write blob", e))?;
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
            Err(e) if e.kind() == ErrorKind::NotFound => Err(Error::NotFound("blob".into())),
            Err(e) => Err(io_err("read blob", e)),
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

    /// B3: a file or symlink pre-planted at the temporary name is never opened or followed;
    /// `create_temp` moves on to the next name.
    #[test]
    fn create_temp_never_follows_a_planted_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, d.path().join(".planted.tmp")).unwrap();
        fs::write(d.path().join(".leftover.tmp"), b"old").unwrap();
        let mut names = [".planted.tmp", ".leftover.tmp", ".fresh.tmp"].into_iter();
        let (p, mut f) = create_temp(d.path(), || Ok(names.next().unwrap().to_string())).unwrap();
        assert_eq!(p, d.path().join(".fresh.tmp"));
        f.write_all(b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        assert_eq!(fs::read(d.path().join(".leftover.tmp")).unwrap(), b"old");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Only planted names: give up instead of looping or following.
        let r = create_temp(d.path(), || Ok(".planted.tmp".to_string()));
        assert!(r.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }

    /// P0 m2: `put` of already-stored content still fsyncs l2, l1 and the root.
    #[tokio::test]
    async fn existing_content_put_syncs_shard_directories() {
        let d = tempfile::tempdir().unwrap();
        // `open` refuses a group/other-writable root, and a pre-existing temp dir takes its mode from
        // the umask (0775 under umask 002): make the mode explicit so the test does not depend on it.
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        FsBlobStore::init(d.path()).await.unwrap();
        let s = FsBlobStore::open(d.path()).await.unwrap();
        let data = Bytes::from_static(b"already here");
        let h = s.put(data.clone()).await.unwrap();
        let l2 = s.path_for(&h).parent().unwrap().to_path_buf();
        let l1 = l2.parent().unwrap().to_path_buf();
        let root = s.root.clone();
        DIR_FSYNCS.lock().unwrap().clear();
        s.put(data).await.unwrap();
        let seen = DIR_FSYNCS.lock().unwrap().clone();
        for want in [&l2, &l1, &root] {
            assert!(seen.contains(want), "not synced: {want:?} in {seen:?}");
        }
    }

    #[test]
    fn temp_names_are_unpredictable() {
        let h = BlobHash::of(b"x");
        let (a, b) = (tmp_name(&h).unwrap(), tmp_name(&h).unwrap());
        assert_ne!(a, b);
        assert!(is_tmp_name(OsStr::new(&a)));
        // `.<64 hex>.<pid>.<n>.<16 hex>.tmp`
        let rand = a.trim_end_matches(".tmp").rsplit('.').next().unwrap();
        assert_eq!(rand.len(), 16);
        assert!(rand.bytes().all(|c| c.is_ascii_hexdigit()));
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
