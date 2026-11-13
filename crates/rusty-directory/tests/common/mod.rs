//! Backend harness for the directory suites: every test body runs against a fresh
//! Postgres database (`RUSTY_TEST_PG_URL`). Raw SQL goes
//! through `rusty_store::fixtures` (feature `test-fixtures`, dev-dependency only); no test holds
//! a pool or a database driver.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use rusty_core::AccountId;
use rusty_directory::Directory;
use rusty_store::fixtures::{Fixtures, SqlFailure};
pub use rusty_store::fixtures::Arg as Bind;
use rusty_store::{BlobStore, FsBlobStore, Gc, Mailbox, Store, StoreConfig};

pub use rusty_store::fixtures::pg_db as pg;

pub struct Backend {
    pub name: &'static str,
    pub dir: Directory,
    pub store: Store,
    pub fx: Fixtures,
    pub tmp: tempfile::TempDir,
    _db: Option<pg::PgTestDb>,
}

// A Drop impl makes closures capture a `Backend` whole (see rusty-store's harness).
impl Drop for Backend {
    fn drop(&mut self) {}
}

impl Backend {
    /// Raw fixture / probe statement returning one `bigint` (`?N` placeholders).
    pub async fn scalar(&self, sql: &str, binds: &[Bind<'_>]) -> i64 {
        self.fx.scalar(sql, binds).await
    }

    pub async fn scalars(&self, sql: &str, binds: &[Bind<'_>]) -> Vec<i64> {
        self.fx.scalars(sql, binds).await
    }

    pub async fn strings(&self, sql: &str, binds: &[Bind<'_>]) -> Vec<String> {
        self.fx.strings(sql, binds).await
    }

    pub async fn byte_rows(&self, sql: &str) -> Vec<Vec<u8>> {
        self.fx.byte_rows(sql, &[]).await
    }

    pub async fn rows(&self, sql: &str, binds: &[Bind<'_>], cols: usize) -> Vec<Vec<String>> {
        self.fx.rows(sql, binds, cols).await
    }

    /// Raw statement without a result; panics if the database rejects it.
    pub async fn run(&self, sql: &str, binds: &[Bind<'_>]) {
        self.fx.run(sql, binds).await;
    }

    /// Raw statement the database may reject.
    pub async fn try_run(&self, sql: &str, binds: &[Bind<'_>]) -> Result<u64, SqlFailure> {
        self.fx.try_run(sql, binds).await
    }

    /// `SELECT count(*) FROM <table>`.
    pub async fn count(&self, table: &str) -> i64 {
        self.fx
            .scalar(&format!("SELECT count(*) FROM {table}"), &[])
            .await
    }

    /// The database clock.
    pub async fn db_now(&self) -> i64 {
        self.fx.db_clock().await
    }

    /// The garbage collector over this backend's repository.
    pub fn gc(&self) -> Gc {
        Gc::new(self.store.repos().gc.clone())
    }

    /// A blob store in a fresh directory of this backend's temp dir.
    pub async fn blobs(&self) -> Arc<dyn BlobStore> {
        let root = self.tmp.path().join("blobs");
        FsBlobStore::init(&root).await.unwrap();
        Arc::new(FsBlobStore::open(&root).await.unwrap())
    }

    /// The mailbox of `acc`, with blobs in this backend's temp dir.
    pub async fn mailbox(&self, acc: AccountId) -> Mailbox {
        let blobs = self.blobs().await;
        Mailbox::open_with(self.store.repos().mailbox.clone(), blobs, acc)
            .await
            .unwrap()
    }

    /// A second, independent database of the same kind.
    pub async fn fresh_like(&self) -> Backend {
        pg_backend().await.expect("postgres backend")
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
