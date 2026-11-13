//! Test fixtures: raw SQL access for the test suites of OTHER crates, without handing them a
//! pool. Compiled only with the `test-fixtures` feature, which may be enabled only from
//! `[dev-dependencies]` (`scripts/check-test-fixtures.sh` enforces that and checks that a release
//! build contains none of this code). Production code never links it.
//!
//! The API is deliberately narrow: statements are plain text with `?` / `?N` placeholders
//! (rewritten to `$N`), arguments are integers, text or bytes, results are integers,
//! strings or text cells. There is no transaction handle and no way to keep a connection.

pub mod pg_db;
mod probe;

pub use probe::StatementProbe;

use crate::Store;
use rusty_core::Error;

/// Marker string that exists only in builds with this feature (checked by the release gate).
pub const FIXTURES_MARKER: &str = "rusty-test-fixtures-marker";

/// Keeps the marker string in the object code of every binary that builds a `Fixtures` (a const
/// used only by tests never lands in a binary: `scripts/check-test-fixtures.sh` greps one).
fn keep_marker() {
    std::hint::black_box(FIXTURES_MARKER);
}

/// One statement argument.
#[derive(Debug, Clone, Copy)]
pub enum Arg<'a> {
    I(i64),
    S(&'a str),
    B(&'a [u8]),
}

/// What a rejected statement tells a test: SQLSTATE and constraint name (Postgres), the message,
/// and what `map_db_error` makes of it (the production mapping).
#[derive(Debug)]
pub struct SqlFailure {
    pub sqlstate: Option<String>,
    pub constraint: Option<String>,
    pub message: String,
    pub mapped: Error,
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
        mapped: crate::map_db_error(e),
    }
}

/// Bare `?` (numbered left to right) and `?N` become `$N`.
pub fn to_dollar_placeholders(sql: &str) -> String {
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
    ($q:expr, $args:expr) => {{
        let mut q = $q;
        for a in $args {
            q = match a {
                Arg::I(v) => q.bind(*v),
                Arg::S(v) => q.bind(*v),
                Arg::B(v) => q.bind(*v),
            };
        }
        q
    }};
}

/// Raw SQL against one Postgres store.
#[derive(Clone)]
pub struct Fixtures {
    /// The store under test (it may run as the unprivileged `rusty_app`): lock and pool holds use it.
    pg: crate::PgStore,
    /// A superuser connection to the same database for raw SQL (see [`Fixtures::admin_of`]).
    admin: Option<crate::PgStore>,
}

/// Run `$body` with `$pool` bound to the pool and `$s` to the SQL text with `$N` placeholders. The raw
/// SQL goes through the admin connection when there is one.
macro_rules! on_backend {
    ($self:ident, $sql:ident, |$pool:ident, $s:ident| $body:expr) => {{
        let store = $self.admin.as_ref().unwrap_or(&$self.pg);
        let $pool = store.pool();
        let converted = to_dollar_placeholders($sql);
        let $s: &str = &converted;
        return $body;
    }};
}

impl Fixtures {
    pub fn of(store: &Store) -> Fixtures {
        keep_marker();
        Fixtures {
            pg: store.postgres().clone(),
            admin: None,
        }
    }

    /// Fixtures for `store` (the store under test) on the test database `db`: raw SQL runs with SUPERUSER
    /// rights, while `hold_write_lock` / `hold_pool` act on the store under test itself. In roles mode
    /// (`RUSTY_TEST_PG_ROLES=1`) that store connects as the unprivileged `rusty_app`, which may not run the
    /// DDL some fixtures need (forcing a failure with a trigger, planting a drifted row).
    pub async fn admin_of(db: &pg_db::PgTestDb, store: &Store) -> Fixtures {
        let mut fx = Fixtures::of(store);
        fx.admin = Some(
            crate::PgStore::connect(db.admin_db_url())
                .await
                .expect("connect the fixture pool with the admin URL"),
        );
        fx
    }

    pub fn from_postgres(store: crate::PgStore) -> Fixtures {
        keep_marker();
        Fixtures {
            pg: store,
            admin: None,
        }
    }

    /// The SQL text as the database receives it.
    pub fn dialect(&self, sql: &str) -> String {
        to_dollar_placeholders(sql)
    }

    /// All rows of a one-column `bigint` query.
    pub async fn scalars(&self, sql: &str, args: &[Arg<'_>]) -> Vec<i64> {
        on_backend!(self, sql, |pool, s| bind_all!(sqlx::query_scalar::<_, i64>(s), args)
            .fetch_all(pool)
            .await
            .expect("fixture query"))
    }

    /// One `bigint`.
    pub async fn scalar(&self, sql: &str, args: &[Arg<'_>]) -> i64 {
        self.scalars(sql, args).await.remove(0)
    }

    /// One nullable `bigint`.
    pub async fn opt_scalar(&self, sql: &str, args: &[Arg<'_>]) -> Option<i64> {
        on_backend!(self, sql, |pool, s| bind_all!(
            sqlx::query_scalar::<_, Option<i64>>(s),
            args
        )
        .fetch_one(pool)
        .await
        .expect("fixture query"))
    }

    /// All rows of a one-column text query.
    pub async fn strings(&self, sql: &str, args: &[Arg<'_>]) -> Vec<String> {
        on_backend!(self, sql, |pool, s| bind_all!(sqlx::query_scalar::<_, String>(s), args)
            .fetch_all(pool)
            .await
            .expect("fixture query"))
    }

    /// All rows of a one-column `bytea`/`blob` query.
    pub async fn byte_rows(&self, sql: &str, args: &[Arg<'_>]) -> Vec<Vec<u8>> {
        on_backend!(self, sql, |pool, s| bind_all!(sqlx::query_scalar::<_, Vec<u8>>(s), args)
            .fetch_all(pool)
            .await
            .expect("fixture query"))
    }

    /// Rows of `cols` text cells (the caller `CAST(.. AS TEXT)`s and aliases the columns);
    /// NULL is the cell `"NULL"`.
    pub async fn rows(&self, sql: &str, args: &[Arg<'_>], cols: usize) -> Vec<Vec<String>> {
        use sqlx::Row;
        on_backend!(self, sql, |pool, s| {
            let rows = bind_all!(sqlx::query(s), args)
                .fetch_all(pool)
                .await
                .expect("fixture query");
            rows.iter()
                .map(|r| {
                    (0..cols)
                        .map(|i| {
                            r.get::<Option<String>, _>(i)
                                .unwrap_or_else(|| "NULL".to_owned())
                        })
                        .collect()
                })
                .collect()
        })
    }

    /// Run a statement that must succeed; returns the rows affected.
    pub async fn run(&self, sql: &str, args: &[Arg<'_>]) -> u64 {
        self.try_run(sql, args)
            .await
            .unwrap_or_else(|e| panic!("fixture statement failed: {e:?}"))
    }

    /// Run a statement the database may reject.
    pub async fn try_run(&self, sql: &str, args: &[Arg<'_>]) -> Result<u64, SqlFailure> {
        on_backend!(self, sql, |pool, s| bind_all!(sqlx::query(s), args)
            .execute(pool)
            .await
            .map(|r| r.rows_affected())
            .map_err(failure))
    }

    /// The database clock, unix seconds.
    pub async fn db_clock(&self) -> i64 {
        self.scalar("SELECT extract(epoch FROM statement_timestamp())::bigint", &[])
            .await
    }
}

/// A held write lock: a transaction holding an `ACCESS EXCLUSIVE` lock on one table. Dropping it rolls back.
pub struct WriteLock {
    _tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl Fixtures {
    /// Block every statement on `table` until the guard drops.
    /// `table` must be a plain lowercase identifier.
    pub async fn hold_write_lock(&self, table: &str) -> WriteLock {
        assert!(table.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
        let mut tx = self.pg.begin_write().await.expect("begin");
        sqlx::query(&format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"))
            .execute(&mut *tx)
            .await
            .expect("lock table");
        WriteLock { _tx: tx }
    }
}

/// Pooled connections held away from the store (Postgres): the pool looks saturated.
pub struct PoolHold {
    _held: Vec<sqlx::pool::PoolConnection<sqlx::Postgres>>,
}

impl Fixtures {
    /// Check out `n` connections of the pool until the guard drops, so every operation of the store has
    /// to wait for a connection.
    pub async fn hold_pool(&self, n: u32) -> PoolHold {
        let mut held = Vec::new();
        for _ in 0..n {
            held.push(self.pg.pool().acquire().await.expect("check out a pooled connection"));
        }
        PoolHold { _held: held }
    }
}
