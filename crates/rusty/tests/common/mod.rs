#![allow(dead_code)]
//! Helpers of the rusty tests.

use std::path::Path;

use rusty_core::config::{Listen, StartupConfig};

/// Dev configuration under `dir` with every listener on an ephemeral loopback port.
pub fn dev_config(dir: &Path) -> StartupConfig {
    let mut c = StartupConfig::dev(dir);
    let any = Listen::on("127.0.0.1:0".parse().unwrap());
    c.smtp.listen_25 = any;
    c.smtp.listen_587 = any;
    c.smtp.listen_465 = any;
    c.imap.listen_143 = any;
    c.imap.listen_993 = any;
    c.limits.shutdown_grace_secs = 5;
    c
}

/// A private (0700) temp directory to hold a data dir.
pub fn private_tmp() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

pub fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}
