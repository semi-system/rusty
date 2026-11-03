//! Backend harness for the directory conformance suite: every test body runs against SQLite
//! and, when `RUSTY_TEST_PG_URL` is set and the `postgres` feature is on, against Postgres.
#![allow(dead_code)]

use std::future::Future;

use rusty_directory::Directory;
use rusty_store::{SqliteStore, Store, StoreConfig};

#[cfg(feature = "postgres")]
#[path = "../../../rusty-store/tests/common/pg.rs"]
pub mod pg;

pub struct Backend {
    pub name: &'static str,
    pub dir: Directory,
    sqlite: Option<SqliteStore>,
    #[cfg(feature = "postgres")]
    pg: Option<rusty_store::PgStore>,
    _tmp: tempfile::TempDir,
    #[cfg(feature = "postgres")]
    _db: Option<pg::PgTestDb>,
}

// A Drop impl makes closures capture a `Backend` whole (see rusty-store's harness).
impl Drop for Backend {
    fn drop(&mut self) {}
}

pub enum Bind<'a> {
    I(i64),
    S(&'a str),
}

impl Backend {
    /// Raw fixture / probe statement returning one `bigint` (`?N` placeholders).
    pub async fn scalar(&self, sql: &str, binds: &[Bind<'_>]) -> i64 {
        if let Some(s) = &self.sqlite {
            let mut q = sqlx::query_scalar::<_, i64>(sql);
            for b in binds {
                q = match b {
                    Bind::I(v) => q.bind(*v),
                    Bind::S(v) => q.bind(*v),
                };
            }
            return q.fetch_one(s.pool()).await.unwrap();
        }
        #[cfg(feature = "postgres")]
        if let Some(p) = &self.pg {
            let sql = sql.replace('?', "$");
            let mut q = sqlx::query_scalar::<_, i64>(&sql);
            for b in binds {
                q = match b {
                    Bind::I(v) => q.bind(*v),
                    Bind::S(v) => q.bind(*v),
                };
            }
            return q.fetch_one(p.pool()).await.unwrap();
        }
        unreachable!()
    }

    /// Raw statement without a result.
    pub async fn run(&self, sql: &str, binds: &[Bind<'_>]) {
        if let Some(s) = &self.sqlite {
            let mut q = sqlx::query(sql);
            for b in binds {
                q = match b {
                    Bind::I(v) => q.bind(*v),
                    Bind::S(v) => q.bind(*v),
                };
            }
            q.execute(s.pool()).await.unwrap();

        }
        #[cfg(feature = "postgres")]
        if let Some(p) = &self.pg {
            let sql = sql.replace('?', "$");
            let mut q = sqlx::query(&sql);
            for b in binds {
                q = match b {
                    Bind::I(v) => q.bind(*v),
                    Bind::S(v) => q.bind(*v),
                };
            }
            q.execute(p.pool()).await.unwrap();
        }
    }

    /// The database clock.
    pub async fn db_now(&self) -> i64 {
        if self.sqlite.is_some() {
            self.scalar("SELECT unixepoch()", &[]).await
        } else {
            self.scalar("SELECT rs_now()", &[]).await
        }
    }

    pub fn is_sqlite(&self) -> bool {
        self.sqlite.is_some()
    }
}

pub async fn sqlite_backend() -> Backend {
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", tmp.path().join("meta.sqlite").display());
    let store = Store::open(StoreConfig::sqlite(url)).await.unwrap();
    Backend {
        name: "sqlite",
        dir: Directory::with_repo(store.repos().directory.clone()),
        sqlite: store.sqlite().cloned(),
        #[cfg(feature = "postgres")]
        pg: None,
        _tmp: tmp,
        #[cfg(feature = "postgres")]
        _db: None,
    }
}

#[cfg(feature = "postgres")]
pub async fn pg_backend() -> Option<Backend> {
    let url = pg::pg_admin_url()?;
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    Some(Backend {
        name: "postgres",
        dir: Directory::with_repo(store.repos().directory.clone()),
        sqlite: None,
        pg: store.postgres().cloned(),
        _tmp: tempfile::tempdir().unwrap(),
        _db: Some(db),
    })
}

pub async fn backends() -> Vec<Backend> {
    #[allow(unused_mut)]
    let mut v = vec![sqlite_backend().await];
    #[cfg(feature = "postgres")]
    if let Some(b) = pg_backend().await {
        v.push(b);
    }
    v
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
