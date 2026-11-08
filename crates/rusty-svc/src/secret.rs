//! `SecretSource`: where key material comes from (TLS key now; DKIM keys and the relay password
//! later). The file implementation fails closed: a secret file must be a regular file owned by
//! the service account, with no group or world access (0600/0400), never a symlink, of bounded
//! size. A future KMS/OpenBao source is another implementation of the trait.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use rusty_core::{Error, Result};
use rustix::fs::{FileType, Mode, OFlags};

/// Largest secret accepted from a file (a PEM private key is well under 10 KiB).
pub const MAX_SECRET_BYTES: u64 = 64 * 1024;

/// Source of secret bytes, named by a path-like locator.
pub trait SecretSource: Send + Sync {
    /// Read the secret `name`. Implementations refuse unsafe storage rather than serve it.
    fn read(&self, name: &Path) -> Result<Vec<u8>>;
}

/// Secrets from files with mode 0600/0400 owned by the running account.
#[derive(Debug, Default, Clone)]
pub struct FileSecretSource;

impl FileSecretSource {
    pub fn new() -> FileSecretSource {
        FileSecretSource
    }
}

impl SecretSource for FileSecretSource {
    fn read(&self, name: &Path) -> Result<Vec<u8>> {
        read_checked(name, 0o077, MAX_SECRET_BYTES, true, "secret file must be mode 0600 (no group or world access)")
    }
}

/// Open `path` without following symlinks, require a regular file of at most `max` bytes that
/// has none of the mode bits in `forbidden` and is owned by the running account (or root unless
/// `owner_must_be_us`), and read it. The checks run on the opened descriptor (no path race).
pub(crate) fn read_checked(
    path: &Path,
    forbidden: u32,
    max: u64,
    owner_must_be_us: bool,
    mode_msg: &str,
) -> Result<Vec<u8>> {
    // O_NONBLOCK so a FIFO cannot hang the start-up; O_NOFOLLOW refuses a symlink.
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|e| match std::io::Error::from(e) {
        e if e.kind() == std::io::ErrorKind::NotFound => Error::NotFound("secret file".into()),
        e if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            Error::Invalid("secret file must not be a symlink".into())
        }
        e => Error::storage_with("cannot open secret file", e),
    })?;
    let st = rustix::fs::fstat(&fd)
        .map_err(|e| Error::storage_with("cannot stat secret file", std::io::Error::from(e)))?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(Error::Invalid("secret file is not a regular file".into()));
    }
    if st.st_mode & forbidden != 0 {
        return Err(Error::Invalid(mode_msg.into()));
    }
    let euid = rustix::process::geteuid().as_raw();
    if st.st_uid != euid && (owner_must_be_us || st.st_uid != 0) {
        return Err(Error::Invalid("secret file is not owned by the service account".into()));
    }
    if st.st_size < 0 || st.st_size as u64 > max {
        return Err(Error::Invalid("secret file is too large".into()));
    }
    let mut buf = Vec::new();
    File::from(fd)
        .take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::storage_with("cannot read secret file", e))?;
    if buf.len() as u64 > max {
        return Err(Error::Invalid("secret file is too large".into()));
    }
    Ok(buf)
}
