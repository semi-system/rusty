//! Backend harness for the rustyctl suites: every test body runs against a fresh Postgres
//! database (`RUSTY_TEST_PG_URL`). Raw access only through
//! `rusty_store::fixtures` (dev-dependency); no test holds a pool or a driver.
#![allow(dead_code)]

use std::future::Future;

use rusty_ctl::{execute, Command, CtlEnv, CtlError, Outcome, PasswordReader, PasswordSource, Secret};
use rusty_directory::Directory;
use rusty_store::fixtures::Fixtures;
use rusty_store::{Store, StoreConfig};

use rusty_store::fixtures::pg_db as pg;

pub struct Backend {
    pub name: &'static str,
    pub dir: Directory,
    pub store: Store,
    pub fx: Fixtures,
    pub tmp: tempfile::TempDir,
    _db: Option<pg::PgTestDb>,
}

impl Drop for Backend {
    fn drop(&mut self) {}
}

/// Passwords for the sources a test uses; panics (a test bug) if asked for more than given.
pub struct FakePasswords {
    pub queue: Vec<String>,
}

impl FakePasswords {
    pub fn new(pw: &[&str]) -> FakePasswords {
        FakePasswords { queue: pw.iter().rev().map(|s| s.to_string()).collect() }
    }
}

impl PasswordReader for FakePasswords {
    fn read(&mut self, _source: PasswordSource) -> Result<Secret, CtlError> {
        let p = self.queue.pop().expect("the command asked for a password the test did not provide");
        Ok(Secret::new(p))
    }
}

impl Backend {
    /// Run one command with the given passwords (in the order they are read).
    pub async fn run(&self, cmd: Command, passwords: &[&str]) -> Result<Outcome, CtlError> {
        self.run_with_host(cmd, passwords, Some("mail.example.net")).await
    }

    pub async fn run_with_host(
        &self,
        cmd: Command,
        passwords: &[&str],
        hostname: Option<&str>,
    ) -> Result<Outcome, CtlError> {
        let mut pw = FakePasswords::new(passwords);
        let mut env = CtlEnv { directory: &self.dir, passwords: &mut pw, hostname, config_note: "test", dkim_dir: None };
        execute(&cmd, &mut env).await
    }

    pub async fn run_with_dkim(&self, cmd: Command, dkim_dir: &std::path::Path) -> Result<Outcome, CtlError> {
        let mut pw = FakePasswords::new(&[]);
        let mut env = CtlEnv { directory: &self.dir, passwords: &mut pw, hostname: Some("mail.example.net"), config_note: "test", dkim_dir: Some(dkim_dir) };
        execute(&cmd, &mut env).await
    }

    pub async fn count(&self, table: &str) -> i64 {
        self.fx.scalar(&format!("SELECT count(*) FROM {table}"), &[]).await
    }
}

pub async fn pg_backend() -> Option<Backend> {
    let url = pg::pg_admin_url()?;
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    Some(Backend {
        name: "postgres",
        dir: Directory::with_repo(store.repos().directory.clone()),
        fx: Fixtures::admin_of(&db, &store).await,
        store,
        tmp: tempfile::tempdir().unwrap(),
        _db: Some(db),
    })
}

pub async fn backends() -> Vec<Backend> {
    vec![pg_backend().await.expect("a Postgres test server (RUSTY_TEST_PG_URL)")]
}

pub async fn for_each_backend<F, Fut>(f: F)
where
    F: Fn(Backend) -> Fut,
    Fut: Future<Output = ()>,
{
    for b in backends().await {
        eprintln!("-- backend {}", b.name);
        f(b).await;
    }
}
