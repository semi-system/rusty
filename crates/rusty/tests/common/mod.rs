#![allow(dead_code)]
//! Helpers of the rusty tests.

use std::path::{Path, PathBuf};

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

use rusty_core::config::{SslMode, StoreSection};
use rusty_store::fixtures::pg_db::{pg_admin_url, PgTestDb};
use rusty_store::{Store, StoreConfig};

/// A fresh Postgres database, initialised with deploy/db/roles.sql and migrated by the migrator role, and what a
/// daemon needs to reach it as the unprivileged runtime role `rusty_app`: the URL without a password and a 0600
/// password file. Keep it alive for the whole test (dropping it drops the database).
pub struct PgDev {
    db: PgTestDb,
    pub url: String,
    pub password: String,
    pw_dir: tempfile::TempDir,
}

pub async fn pg_dev() -> PgDev {
    use std::os::unix::fs::PermissionsExt;
    let admin = pg_admin_url().expect("a Postgres test server (RUSTY_TEST_PG_URL)");
    let db = PgTestDb::create_with_roles(&admin).await;
    let (url, password) = db.split_url();
    let pw_dir = private_tmp();
    let file = pw_dir.path().join("pg-password");
    std::fs::write(&file, &password).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    PgDev { db, url, password, pw_dir }
}

impl PgDev {
    /// The dev configuration under `dir` with `[store]` pointing at this database.
    pub fn config(&self, dir: &Path) -> StartupConfig {
        let mut c = dev_config(dir);
        c.store = StoreSection {
            url: Some(self.url.clone()),
            password_file: Some(self.pw_dir.path().join("pg-password")),
            sslmode: SslMode::Disable,
            // The test server has no TLS: production accepts that only with this explicit, logged override.
            insecure_allow_unverified_tls: true,
            password_from_dev_env: false,
            ..StoreSection::default()
        };
        c
    }

    /// The 0600 file that holds the password (for a configuration file's `password_file`).
    pub fn password_file(&self) -> PathBuf {
        self.pw_dir.path().join("pg-password")
    }

    /// A store over this database (the migrated schema is already there).
    pub async fn open_store(&self) -> Store {
        Store::open(StoreConfig::postgres(self.db.url())).await.unwrap()
    }
}
