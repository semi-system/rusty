//! Backend harness shared by the PG2 spike tests (conformance, Postgres-only, measurements).
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use rusty_core::PlanId;
use rusty_core::{AccountId, DomainId, Error, FolderId};
use rusty_store::repo::sqlite::SqliteRepo;
use rusty_store::repo::{DirectoryRepo, DnsRepo, GcRepo, JobRepo, MailboxRepo, NewAccount, Resolution};
use rusty_store::{BlobStore, SqliteStore};
use rusty_store::{DnsStore, Gc, JobQueue, Mailbox};

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
    pub jrepo: Arc<dyn JobRepo>,
    pub nrepo: Arc<dyn DnsRepo>,
    pub grepo: Arc<dyn GcRepo>,
    pub blobs: Arc<dyn BlobStore>,
    pub sqlite: Option<SqliteStore>,
    #[cfg(feature = "postgres")]
    pub pg: Option<(PgStore, PgRepo)>,
    pub _tmp: tempfile::TempDir,
    #[cfg(feature = "postgres")]
    pub _db: Option<PgTestDb>,
    /// Superuser pool of the Postgres test database: raw fixture SQL (setup, fault injection, catalog
    /// probes) is not part of the system under test, so it runs with full rights even when the
    /// repositories run as the unprivileged `rusty_app` (RUSTY_TEST_PG_ROLES=1).
    #[cfg(feature = "postgres")]
    pub _fixture: Option<sqlx::PgPool>,
}

#[cfg(feature = "postgres")]
impl Backend {
    pub fn fx(&self) -> &sqlx::PgPool {
        self._fixture.as_ref().expect("a Postgres backend has a fixture pool")
    }
}

pub async fn blob_dir(tmp: &tempfile::TempDir) -> Arc<dyn BlobStore> {
    Arc::new(super::blob_store(&tmp.path().join("blobs")).await)
}

pub async fn sqlite_backend() -> Backend {
    let tmp = tempfile::tempdir().unwrap();
    let store = super::file_store(tmp.path()).await;
    sqlite_backend_over(store, tmp).await
}

/// SQLite over a private in-memory database (one connection) with blobs in a temp dir.
pub async fn sqlite_mem_backend() -> Backend {
    let tmp = tempfile::tempdir().unwrap();
    sqlite_backend_over(super::mem().await, tmp).await
}

async fn sqlite_backend_over(store: SqliteStore, tmp: tempfile::TempDir) -> Backend {
    let repo = Arc::new(SqliteRepo::new(store.clone()));
    Backend {
        name: "sqlite",
        dir: TestDir::new(repo.clone()),
        drepo: repo.clone(),
        mrepo: repo.clone(),
        jrepo: repo.clone(),
        nrepo: repo.clone(),
        grepo: repo,
        blobs: blob_dir(&tmp).await,
        sqlite: Some(store),
        #[cfg(feature = "postgres")]
        pg: None,
        _tmp: tmp,
        #[cfg(feature = "postgres")]
        _db: None,
        #[cfg(feature = "postgres")]
        _fixture: None,
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
        mrepo: arc.clone(),
        jrepo: arc.clone(),
        nrepo: arc.clone(),
        grepo: arc,
        blobs: blob_dir(&tmp).await,
        sqlite: None,
        pg: Some((store, repo)),
        _tmp: tmp,
        _fixture: Some(sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(db.admin_db_url()).await.unwrap()),
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
    let acc = b
        .dir
        .create_account("alice", dom, None, None)
        .await
        .unwrap();
    let mb = Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc)
        .await
        .unwrap();
    (dom, acc, mb)
}

/// Raw fixture write (the slice has no alias/list writer): `?N` placeholders, i64/str binds.
pub enum Bind<'a> {
    I(i64),
    S(&'a str),
    B(&'a [u8]),
}

pub async fn exec(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> i64 {
    // Every fixture statement ends in `RETURNING id`.
    if let Some(s) = &b.sqlite {
        let mut q = sqlx::query_scalar::<_, i64>(sql);
        for x in binds {
            q = match x {
                Bind::I(v) => q.bind(*v),
                Bind::S(v) => q.bind(*v),
                Bind::B(v) => q.bind(*v),
            };
        }
        return q.fetch_one(s.pool()).await.unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        let sql = dialect_sql(b, sql);
        let mut q = sqlx::query_scalar::<_, i64>(&sql);
        for x in binds {
            q = match x {
                Bind::I(v) => q.bind(*v),
                Bind::S(v) => q.bind(*v),
                Bind::B(v) => q.bind(*v),
            };
        }
        return q.fetch_one(b.fx()).await.unwrap();
    }
    unreachable!()
}

/// Dialect-neutral placeholders: bare `?` (numbered left to right) and `?N` become `$N` on
/// Postgres and stay as written on SQLite (which accepts both forms).
pub fn dialect_sql(b: &Backend, sql: &str) -> String {
    if b.sqlite.is_some() {
        return sql.to_owned();
    }
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    let mut it = sql.chars().peekable();
    while let Some(c) = it.next() {
        if c != '?' {
            out.push(c);
            continue;
        }
        let mut digits = String::new();
        while let Some(d) = it.peek().filter(|d| d.is_ascii_digit()) {
            digits.push(*d);
            it.next();
        }
        if digits.is_empty() {
            n += 1;
            out.push_str(&format!("${n}"));
        } else {
            out.push_str(&format!("${digits}"));
        }
    }
    out
}

macro_rules! bind_all {
    ($q:expr, $binds:expr) => {{
        let mut q = $q;
        for x in $binds {
            q = match x {
                Bind::I(v) => q.bind(*v),
                Bind::S(v) => q.bind(*v),
                Bind::B(v) => q.bind(*v),
            };
        }
        q
    }};
}

/// Rows of a one-column `bigint` fixture query with binds.
pub async fn fetch_i64s(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> Vec<i64> {
    let sql = dialect_sql(b, sql);
    if let Some(s) = &b.sqlite {
        return bind_all!(sqlx::query_scalar::<_, i64>(&sql), binds)
            .fetch_all(s.pool())
            .await
            .unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return bind_all!(sqlx::query_scalar::<_, i64>(&sql), binds)
            .fetch_all(b.fx())
            .await
            .unwrap();
    }
    unreachable!()
}

/// Rows of a one-column text fixture query with binds.
pub async fn fetch_strings(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> Vec<String> {
    let sql = dialect_sql(b, sql);
    if let Some(s) = &b.sqlite {
        return bind_all!(sqlx::query_scalar::<_, String>(&sql), binds)
            .fetch_all(s.pool())
            .await
            .unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return bind_all!(sqlx::query_scalar::<_, String>(&sql), binds)
            .fetch_all(b.fx())
            .await
            .unwrap();
    }
    unreachable!()
}

/// One nullable `bigint` (`None` when the column is NULL).
pub async fn fetch_opt_i64(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> Option<i64> {
    let sql = dialect_sql(b, sql);
    if let Some(s) = &b.sqlite {
        return bind_all!(sqlx::query_scalar::<_, Option<i64>>(&sql), binds)
            .fetch_one(s.pool())
            .await
            .unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return bind_all!(sqlx::query_scalar::<_, Option<i64>>(&sql), binds)
            .fetch_one(b.fx())
            .await
            .unwrap();
    }
    unreachable!()
}

/// Rows of a multi-column fixture query as text cells. The caller `CAST(.. AS TEXT)`s every
/// column and parses the cells (the text form of integers is identical on both backends).
pub async fn fetch_rows(
    b: &Backend,
    sql: &str,
    binds: &[Bind<'_>],
    cols: usize,
) -> Vec<Vec<String>> {
    use sqlx::Row;
    let sql = dialect_sql(b, sql);
    if let Some(s) = &b.sqlite {
        let rows = bind_all!(sqlx::query(&sql), binds)
            .fetch_all(s.pool())
            .await
            .unwrap();
        return rows
            .iter()
            .map(|r| {
                (0..cols)
                    .map(|i| {
                        r.get::<Option<String>, _>(i)
                            .unwrap_or_else(|| "NULL".into())
                    })
                    .collect()
            })
            .collect();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        let rows = bind_all!(sqlx::query(&sql), binds)
            .fetch_all(b.fx())
            .await
            .unwrap();
        return rows
            .iter()
            .map(|r| {
                (0..cols)
                    .map(|i| {
                        r.get::<Option<String>, _>(i)
                            .unwrap_or_else(|| "NULL".into())
                    })
                    .collect()
            })
            .collect();
    }
    unreachable!()
}

/// Run one statement with binds, no result; returns the rows affected.
pub async fn execute(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> u64 {
    let sql = dialect_sql(b, sql);
    if let Some(s) = &b.sqlite {
        return bind_all!(sqlx::query(&sql), binds)
            .execute(s.pool())
            .await
            .unwrap()
            .rows_affected();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return bind_all!(sqlx::query(&sql), binds)
            .execute(b.fx())
            .await
            .unwrap()
            .rows_affected();
    }
    unreachable!()
}

/// What a rejected fixture statement tells a test: the SQLSTATE and constraint name on
/// Postgres, the extended result code and message on SQLite.
#[derive(Debug)]
pub struct SqlFailure {
    pub sqlstate: Option<String>,
    pub constraint: Option<String>,
    pub message: String,
    /// What `rusty_store::map_db_error` makes of it (the production mapping).
    pub mapped: rusty_core::Error,
}

fn failure(e: sqlx::Error) -> SqlFailure {
    let (sqlstate, constraint, message) = match e.as_database_error() {
        Some(d) => (
            d.code().map(|c| c.to_string()),
            d.constraint().map(str::to_owned),
            d.message().to_owned(),
        ),
        None => (None, None, e.to_string()),
    };
    SqlFailure {
        sqlstate,
        constraint,
        message,
        mapped: rusty_store::map_db_error(e),
    }
}

/// Like [`execute`], but returns the database's rejection instead of panicking.
pub async fn try_execute(b: &Backend, sql: &str, binds: &[Bind<'_>]) -> Result<u64, SqlFailure> {
    let sql = dialect_sql(b, sql);
    if let Some(s) = &b.sqlite {
        return bind_all!(sqlx::query(&sql), binds)
            .execute(s.pool())
            .await
            .map(|r| r.rows_affected())
            .map_err(failure);
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return bind_all!(sqlx::query(&sql), binds)
            .execute(b.fx())
            .await
            .map(|r| r.rows_affected())
            .map_err(failure);
    }
    unreachable!()
}

/// Make every INSERT into `table` fail (fault injection for atomicity tests).
pub async fn inject_insert_failure(b: &Backend, table: &str) {
    assert!(table.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
    if b.sqlite.is_some() {
        run(
            b,
            &format!(
                "CREATE TRIGGER inject_fail BEFORE INSERT ON {table} \
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
            ),
        )
        .await;
    } else {
        run(
            b,
            "CREATE FUNCTION inject_fail_fn() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$",
        )
        .await;
        run(
            b,
            &format!(
                "CREATE TRIGGER inject_fail BEFORE INSERT ON {table} \
                 FOR EACH ROW EXECUTE FUNCTION inject_fail_fn()"
            ),
        )
        .await;
    }
}

/// Undo [`inject_insert_failure`].
pub async fn clear_injected_failure(b: &Backend, table: &str) {
    if b.sqlite.is_some() {
        run(b, "DROP TRIGGER inject_fail").await;
    } else {
        run(b, &format!("DROP TRIGGER inject_fail ON {table}")).await;
        run(b, "DROP FUNCTION inject_fail_fn()").await;
    }
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
        return sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(s.pool())
            .await
            .unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(b.fx())
            .await
            .unwrap();
    }
    unreachable!()
}

/// All rows of a one-column `bigint` fixture query.
pub async fn scalars(b: &Backend, sql: &str) -> Vec<i64> {
    if let Some(s) = &b.sqlite {
        return sqlx::query_scalar::<_, i64>(sql)
            .fetch_all(s.pool())
            .await
            .unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return sqlx::query_scalar::<_, i64>(sql)
            .fetch_all(b.fx())
            .await
            .unwrap();
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

// ------------------------------------------------------------------ jobs and GC (PG5)

impl Backend {
    /// A job queue over this backend's repository.
    pub fn jobs(&self) -> JobQueue {
        JobQueue::with_repo(self.jrepo.clone())
    }

    /// A job queue with smaller pending-job caps.
    pub fn jobs_with_limits(&self, per_domain: u32, no_domain: u32) -> JobQueue {
        JobQueue::with_repo_and_limits(self.jrepo.clone(), per_domain, no_domain)
    }

    /// The DNS validation store over this backend's repository.
    pub fn dns(&self) -> DnsStore {
        DnsStore::with_repo(self.nrepo.clone())
    }

    /// The garbage collector over this backend's repository.
    pub fn gc(&self) -> Gc {
        Gc::new(self.grepo.clone())
    }
}

/// Run one fixture statement with no parameters and no result (dialect-neutral SQL text).
pub async fn run(b: &Backend, sql: &str) {
    if let Some(s) = &b.sqlite {
        sqlx::query(sql).execute(s.pool()).await.unwrap();
        return;
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        sqlx::query(sql).execute(b.fx()).await.unwrap();
        return;
    }
    unreachable!()
}

/// One text value from a fixture query.
pub async fn text(b: &Backend, sql: &str) -> String {
    if let Some(s) = &b.sqlite {
        return sqlx::query_scalar::<_, String>(sql)
            .fetch_one(s.pool())
            .await
            .unwrap();
    }
    #[cfg(feature = "postgres")]
    if b.pg.is_some() {
        return sqlx::query_scalar::<_, String>(sql)
            .fetch_one(b.fx())
            .await
            .unwrap();
    }
    unreachable!()
}

/// The database clock, read through a fixture session (unix seconds).
pub async fn db_clock(b: &Backend) -> i64 {
    if b.sqlite.is_some() {
        scalar(b, "SELECT unixepoch()").await
    } else {
        scalar(
            b,
            "SELECT extract(epoch FROM statement_timestamp())::bigint",
        )
        .await
    }
}

/// SQL literal of a blob hash (`BlobHash` hex) for this dialect: text on SQLite, `bytea` on Postgres.
pub fn hash_lit(b: &Backend, hex: &str) -> String {
    assert!(hex.bytes().all(|c| c.is_ascii_hexdigit()));
    if b.sqlite.is_some() {
        format!("'{hex}'")
    } else {
        format!("decode('{hex}', 'hex')")
    }
}

/// Insert an unreferenced `blobs` row (refcount 0; the trigger stamps `zero_since` with the DB clock).
pub async fn insert_blob_row(b: &Backend, hex: &str, size: i64) {
    run(
        b,
        &format!(
            "INSERT INTO blobs (hash, size) VALUES ({}, {size})",
            hash_lit(b, hex)
        ),
    )
    .await;
}

pub async fn set_zero_since(b: &Backend, hex: &str, t: i64) {
    run(
        b,
        &format!(
            "UPDATE blobs SET zero_since = {t} WHERE hash = {}",
            hash_lit(b, hex)
        ),
    )
    .await;
}

pub async fn blob_row_exists(b: &Backend, hex: &str) -> bool {
    scalar(
        b,
        &format!(
            "SELECT count(*) FROM blobs WHERE hash = {}",
            hash_lit(b, hex)
        ),
    )
    .await
        == 1
}

/// A `GcRepo` whose database clock is skewed: everything is delegated, `db_now` is shifted.
/// Proves that the grace cutoff is read from the REPOSITORY (the database), not from the
/// process clock.
pub struct SkewedGc {
    pub inner: Arc<dyn GcRepo>,
    pub skew: i64,
}

#[async_trait::async_trait]
impl GcRepo for SkewedGc {
    async fn db_now(&self) -> Result<i64, Error> {
        Ok(self.inner.db_now().await? + self.skew)
    }
    async fn blob_candidates(
        &self,
        cutoff_secs: i64,
        after: Option<&rusty_core::BlobHash>,
        limit: i64,
    ) -> Result<Vec<rusty_core::BlobHash>, Error> {
        self.inner.blob_candidates(cutoff_secs, after, limit).await
    }
    async fn delete_blob_rows(
        &self,
        hashes: &[rusty_core::BlobHash],
        cutoff_secs: i64,
    ) -> Result<Vec<rusty_core::BlobHash>, Error> {
        self.inner.delete_blob_rows(hashes, cutoff_secs).await
    }
    async fn blob_rows_in_shard(&self, shard: u8) -> Result<Vec<String>, Error> {
        self.inner.blob_rows_in_shard(shard).await
    }
    async fn blob_row_exists(&self, hash: &rusty_core::BlobHash) -> Result<bool, Error> {
        self.inner.blob_row_exists(hash).await
    }
    async fn purge_deleted_batch(&self, o: i64, l: i64) -> Result<rusty_store::PurgeBatch, Error> {
        self.inner.purge_deleted_batch(o, l).await
    }
    async fn purge_deliveries_batch(&self, o: i64, l: i64) -> Result<u64, Error> {
        self.inner.purge_deliveries_batch(o, l).await
    }
    async fn purge_capabilities_batch(&self, o: i64, l: i64) -> Result<u64, Error> {
        self.inner.purge_capabilities_batch(o, l).await
    }
}

/// The Postgres backend, or `None` (feature off, or `RUSTY_TEST_PG_URL` unset: the skip notice
/// is printed by `pg_admin_url`).
pub async fn pg_backend_or_skip() -> Option<Backend> {
    #[cfg(feature = "postgres")]
    {
        pg_backend().await
    }
    #[cfg(not(feature = "postgres"))]
    {
        eprintln!("POSTGRES_TESTS_SKIPPED: built without the postgres feature");
        None
    }
}

/// A second, independent database of the same kind as `b`.
pub async fn fresh_like(b: &Backend) -> Backend {
    if b.sqlite.is_some() {
        return sqlite_backend().await;
    }
    pg_backend_or_skip().await.expect("postgres backend")
}

impl Backend {
    pub async fn count_jobs(&self) -> i64 {
        scalar(self, "SELECT count(*) FROM jobs").await
    }
}
