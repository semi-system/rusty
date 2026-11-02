//! Startup configuration (spec §6): a small TOML file read once at process start.
//!
//! Error texts never echo file contents (they may sit next to secrets).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};
use serde::Deserialize;

use crate::{Error, Result};

/// Largest configuration file accepted by [`StartupConfig::load`].
pub const MAX_CONFIG_FILE_BYTES: u64 = 64 * 1024;
const MIN_MESSAGE_BYTES: u64 = 1024;
const MAX_MESSAGE_BYTES: u64 = 1024 * 1024 * 1024;
const MIN_GC_GRACE_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupConfig {
    pub database_path: PathBuf,
    pub blob_dir: PathBuf,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default = "default_max_message_bytes")]
    pub max_message_bytes: u64,
    #[serde(default = "default_gc_grace_seconds")]
    pub gc_grace_seconds: u64,
}

fn default_max_message_bytes() -> u64 {
    52_428_800
}

fn default_gc_grace_seconds() -> u64 {
    86_400
}

impl StartupConfig {
    /// Parse and validate. Syntax and schema errors are `Invalid` without the offending text.
    pub fn from_toml_str(s: &str) -> Result<StartupConfig> {
        let c: StartupConfig = toml::from_str(s)
            .map_err(|_| Error::Invalid("configuration is not valid (syntax or schema)".into()))?;
        c.validate()?;
        Ok(c)
    }

    /// Read at most [`MAX_CONFIG_FILE_BYTES`] from `path` and parse it.
    pub fn load(path: &Path) -> Result<StartupConfig> {
        // O_NONBLOCK so opening a FIFO (or similar) cannot hang the process; the fstat on the
        // opened descriptor (no path race) then rejects anything that is not a regular file.
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| match std::io::Error::from(e) {
            e if e.kind() == std::io::ErrorKind::NotFound => {
                Error::NotFound("configuration file".into())
            }
            e => Error::storage_with("cannot open configuration file", e),
        })?;
        let st = rustix::fs::fstat(&fd)
            .map_err(|e| Error::storage_with("cannot stat configuration file", std::io::Error::from(e)))?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
            || st.st_size < 0
            || st.st_size as u64 > MAX_CONFIG_FILE_BYTES
        {
            return Err(Error::Invalid("configuration file is not a small regular file".into()));
        }
        let file = File::from(fd);
        // Bounded even if the file grows after the check.
        let mut buf = Vec::new();
        file.take(MAX_CONFIG_FILE_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::storage_with("cannot read configuration file", e))?;
        if buf.len() as u64 > MAX_CONFIG_FILE_BYTES {
            return Err(Error::Invalid("configuration file is too large".into()));
        }
        let text = String::from_utf8(buf)
            .map_err(|_| Error::Invalid("configuration file is not UTF-8".into()))?;
        Self::from_toml_str(&text)
    }

    fn validate(&self) -> Result<()> {
        if !self.database_path.is_absolute() {
            return Err(Error::Invalid("database_path must be absolute".into()));
        }
        if !self.blob_dir.is_absolute() {
            return Err(Error::Invalid("blob_dir must be absolute".into()));
        }
        if !(MIN_MESSAGE_BYTES..=MAX_MESSAGE_BYTES).contains(&self.max_message_bytes) {
            return Err(Error::Invalid("max_message_bytes must be 1 KiB..=1 GiB".into()));
        }
        if self.gc_grace_seconds < MIN_GC_GRACE_SECONDS {
            return Err(Error::Invalid("gc_grace_seconds must be at least 3600".into()));
        }
        Ok(())
    }
}
