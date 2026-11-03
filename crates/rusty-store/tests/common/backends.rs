//! Backend harness shared by the PG2 spike tests (conformance, Postgres-only, measurements).
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use rusty_core::{AccountId, DomainId, Error, FolderId};
use rusty_store::repo::sqlite::SqliteRepo;
use rusty_store::repo::{DirectoryRepo, MailboxRepo, NewAccount, Resolution};
use rusty_store::Mailbox;
use rusty_core::PlanId;
use rusty_store::{BlobStore, SqliteStore};

#[cfg(feature = "postgres")]
use super::pg::{pg_admin_url, PgTestDb};
#[cfg(feature = "postgres")]
use rusty_store::repo::pg::PgRepo;
#[cfg(feature = "postgres")]
use rusty_store::PgStore;

pub const INBOX: FolderId = FolderId(1);

pub struct Backend {
    pub name: &'static str,
    pub dir: TestDir,
    pub drepo: Arc<dyn DirectoryRepo>,
    pub mrepo: Arc<dyn MailboxRepo>,
    pub blobs: Arc<dyn BlobStore>,
    pub sqlite: Option<SqliteStore>,
    #[cfg(feature = "postgres")]
    pub pg: Option<(PgStore, PgRepo)>,
    pub _tmp: tempfile::TempDir,
    #[cfg(feature = "postgres")]
    pub _db: Option<PgTestDb>,
}

pub async fn blob_dir(tmp: &tempfile::TempDir) -> Arc<dyn BlobStore> {
    Arc::new(super::blob_store(&tmp.path().join("blobs")).await)
}

pub async fn sqlite_backend() -> Backend {
    let tmp = tempfile::tempdir().unwrap();
    let store = super::file_store(tmp.path()).await;
    let repo = Arc::new(SqliteRepo::new(store.clone()));
    Backend {
        name: "sqlite",
        dir: TestDir::new(repo.clone()),
        drepo: repo.clone(),
        mrepo: repo,
        blobs: blob_dir(&tmp).await,
        sqlite: Some(store),
        #[cfg(feature = "postgres")]
        pg: None,
        _tmp: tmp,
        #[cfg(feature = "postgres")]
        _db: None,
    }
}

#[cfg(feature = "postgres")]
pub async fn pg_backend() -> Option<Backend> {
    let url = pg_admin_url()?;
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let repo = PgRepo::new(store.clone());
    let arc = Arc::new(repo.clone());
    let tmp = tempfile::tempdir().unwrap();
    Some(Backend {
        name: "postgres",
        dir: TestDir::new(arc.clone()),
        drepo: arc.clone(),
        mrepo: arc,
        blobs: blob_dir(&tmp).await,
        sqlite: None,
        pg: Some((store, repo)),
        _tmp: tmp,
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

/// Domain `<tag>.test` plus account `alice` in it, and the opened mailbox.
pub async fn acct(b: &Backend, tag: &str) -> (DomainId, AccountId, Mailbox) {
    let dom = b.dir.create_domain(&format!("{tag}.test")).await.unwrap();
    let acc = b.dir.create_account("alice", dom, None, None).await.unwrap();
    let mb = Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc)
        .await
        .unwrap();
    (dom, acc, mb)
}

/// Raw fixture write (the slice has no alias/list writer): `?N` placeholders, i64/str binds.
pub enum Bind<'a> {
    I(i64),
    S(&'a str),
}

pub async fn exec(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> i64 {
    // Every fixture statement ends in `RETURNING id`.
    if let Some(s) = &b.sqlite {
        let mut q = sqlx::query_scalar::<_, i64>(sql);
        for x in binds {
            q = match x {
                Bind::I(v) => q.bind(*v),
                Bind::S(v) => q.bind(*v),
            };
        }
        return q.fetch_one(s.pool()).await.unwrap();
    }
    #[cfg(feature = "postgres")]
    if let Some((p, _)) = &b.pg {
        let sql = sql.replace('?', "$");
        let mut q = sqlx::query_scalar::<_, i64>(&sql);
        for x in binds {
            q = match x {
                Bind::I(v) => q.bind(*v),
                Bind::S(v) => q.bind(*v),
            };
        }
        return q.fetch_one(p.pool()).await.unwrap();
    }
    unreachable!()
}

/// A further account in an existing domain, with its mailbox.
pub async fn acct_in(b: &Backend, dom: DomainId, local: &str) -> Mailbox {
    let acc = b.dir.create_account(local, dom, None, None).await.unwrap();
    Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc)
        .await
        .unwrap()
}

/// One `bigint` from a fixture query (no parameters; use `count(*)`, never `sum`).
pub async fn scalar(b: &Backend, sql: &str) -> i64 {
    if let Some(s) = &b.sqlite {
        return sqlx::query_scalar::<_, i64>(sql).fetch_one(s.pool()).await.unwrap();
    }
    #[cfg(feature = "postgres")]
    if let Some((p, _)) = &b.pg {
        return sqlx::query_scalar::<_, i64>(sql).fetch_one(p.pool()).await.unwrap();
    }
    unreachable!()
}

/// All rows of a one-column `bigint` fixture query.
pub async fn scalars(b: &Backend, sql: &str) -> Vec<i64> {
    if let Some(s) = &b.sqlite {
        return sqlx::query_scalar::<_, i64>(sql).fetch_all(s.pool()).await.unwrap();
    }
    #[cfg(feature = "postgres")]
    if let Some((p, _)) = &b.pg {
        return sqlx::query_scalar::<_, i64>(sql).fetch_all(p.pool()).await.unwrap();
    }
    unreachable!()
}

pub fn is_conflict<T: std::fmt::Debug>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::Conflict(_)))
}

// ------------------------------------------------------------------ directory


// A `Drop` impl makes closures capture a `Backend` as a whole. Without it, an `async move`
// block that names only `b.dir` (edition-2021 disjoint capture) leaves `_tmp` behind in the
// closure parameter, which is dropped at once: the temp dir (and the SQLite file) vanishes
// under the test ("attempt to write a readonly database").
impl Drop for Backend {
    fn drop(&mut self) {}
}

/// Minimal directory facade for repository-level tests: the stand-in validation that
/// `rusty_directory::Directory` does with IDNA and the local-part policy (rusty-store cannot
/// depend on rusty-directory).
#[derive(Clone)]
pub struct TestDir {
    repo: Arc<dyn DirectoryRepo>,
}

impl TestDir {
    pub fn new(repo: Arc<dyn DirectoryRepo>) -> Self {
        Self { repo }
    }

    pub async fn create_domain(&self, name: &str) -> Result<DomainId, Error> {
        let ok = !name.is_empty()
            && name.len() <= 253
            && name
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'));
        if !ok {
            return Err(Error::Invalid("invalid domain".into()));
        }
        self.repo.create_domain(name).await
    }

    pub async fn create_account(
        &self,
        local: &str,
        domain: DomainId,
        password_hash: Option<String>,
        plan: Option<PlanId>,
    ) -> Result<AccountId, Error> {
        if local.is_empty() || local.len() > 64 {
            return Err(Error::Invalid("invalid local part".into()));
        }
        self.repo
            .create_account(&NewAccount {
                local: local.to_owned(),
                domain,
                plan,
                password_hash,
            })
            .await
    }

    pub async fn resolve(&self, local: &str, domain: &str) -> Result<Resolution, Error> {
        self.repo
            .resolve(local, domain, 10_000)
            .await?
            .ok_or_else(|| Error::NotFound("address".into()))
    }
}
