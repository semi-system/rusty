//! Postgres schema management: the explicit migration step, the schema-version check the daemon
//! runs at connect time, and the runtime-role check.
//!
//! * `migrate_postgres` is the ONLY code path that changes the schema. It is used by
//!   `rustyctl db migrate` with the migrator role's credentials, connects DIRECTLY to Postgres (sqlx
//!   takes a session-level advisory lock around the migrations, which a transaction-pooling proxy
//!   would break) and ends by applying the privilege matrix `grants_pg.sql`.
//! * `require_current_schema` makes the daemon refuse to run when the database is not at exactly
//!   the version this binary embeds (older, newer, a failed or an edited migration).
//! * `verify_runtime_role` makes the daemon refuse a role that is more powerful than the runtime
//!   role should be.

use rusty_core::{Error, Result};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::pg::{connect_options, PgLimits, PgSecurity};

/// The embedded Postgres migrations (version numbers equal the SQLite twins from 0010 on).
static MIGRATOR: Migrator = sqlx::migrate!("./migrations_pg");

/// Privilege matrix applied after the migrations (see the file header).
const GRANTS_SQL: &str = include_str!("../grants_pg.sql");

/// Role names of deploy/db/roles.sql.
pub const APP_ROLE: &str = "rusty_app";
pub const MIGRATOR_ROLE: &str = "rusty_migrator";
pub const RO_ROLE: &str = "rusty_ro";

/// Where the database is relative to this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// Every embedded migration is applied, unmodified, and nothing else.
    Current,
    /// No migration table: `db migrate` was never run.
    Uninitialised,
    /// Some embedded migrations are not applied yet.
    Behind,
    /// The database has migrations this binary does not know (it was migrated by a newer one).
    Ahead,
    /// A migration is recorded as failed.
    Dirty,
    /// An applied migration differs from the embedded one (edited after it was applied).
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaStatus {
    pub state: SchemaState,
    /// Versions applied successfully, ascending.
    pub applied: Vec<i64>,
    /// Versions this binary embeds, ascending.
    pub expected: Vec<i64>,
}

impl SchemaStatus {
    /// Versions this binary embeds that the database lacks.
    pub fn pending(&self) -> Vec<i64> {
        self.expected.iter().copied().filter(|v| !self.applied.contains(v)).collect()
    }
}

/// Result of [`migrate_postgres`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub before: SchemaStatus,
    pub after: SchemaStatus,
    /// Whether the roles of deploy/db/roles.sql existed and the privilege matrix was applied.
    pub grants_applied: bool,
}

fn expected_versions() -> Vec<i64> {
    let mut v: Vec<i64> = MIGRATOR.migrations.iter().map(|m| m.version).collect();
    v.sort_unstable();
    v
}

/// Versions of the embedded migrations (for the SQLite/PG numbering check and `db status`).
pub fn embedded_versions() -> Vec<i64> {
    expected_versions()
}

fn q_err(_: sqlx::Error) -> Error {
    Error::storage("cannot read the Postgres schema state")
}

/// Compare the migration table with the embedded migrations.
pub(crate) async fn schema_status_of(pool: &PgPool) -> Result<SchemaStatus> {
    let expected = expected_versions();
    let present: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations')::text").fetch_one(pool).await.map_err(q_err)?;
    if present.is_none() {
        return Ok(SchemaStatus { state: SchemaState::Uninitialised, applied: Vec::new(), expected });
    }
    let rows: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .map_err(q_err)?;
    let applied: Vec<i64> = rows.iter().filter(|r| r.1).map(|r| r.0).collect();
    let state = if rows.iter().any(|r| !r.1) {
        SchemaState::Dirty
    } else if rows.iter().any(|(v, _, _)| !expected.contains(v)) {
        SchemaState::Ahead
    } else if rows.iter().any(|(v, _, sum)| {
        MIGRATOR.migrations.iter().any(|m| m.version == *v && m.checksum.as_ref() != sum.as_slice())
    }) {
        SchemaState::Mismatch
    } else if expected.iter().any(|v| !applied.contains(v)) {
        SchemaState::Behind
    } else {
        SchemaState::Current
    };
    Ok(SchemaStatus { state, applied, expected })
}

/// The daemon's start-up check: refuse unless the schema is exactly this binary's.
pub(crate) async fn require_current_schema(pool: &PgPool) -> Result<()> {
    let s = schema_status_of(pool).await?;
    let newest = |v: &[i64]| v.last().copied().unwrap_or(0);
    match s.state {
        SchemaState::Current => Ok(()),
        SchemaState::Uninitialised => {
            Err(Error::storage("database schema is not initialised: run `rustyctl db migrate`"))
        }
        SchemaState::Behind => Err(Error::storage(format!(
            "database schema is older than this binary (applied {}, expected {}): run `rustyctl db migrate`",
            newest(&s.applied),
            newest(&s.expected)
        ))),
        SchemaState::Ahead => Err(Error::storage(format!(
            "database schema is newer than this binary (applied {}, this binary knows {}): run a matching or newer release",
            newest(&s.applied),
            newest(&s.expected)
        ))),
        SchemaState::Dirty | SchemaState::Mismatch => Err(Error::storage(
            "database schema history is inconsistent (a failed or modified migration): see `rustyctl db status`",
        )),
    }
}

fn role_err(what: &str) -> Error {
    Error::Invalid(format!("the runtime database role {what}"))
}

/// Refuse a role with more power than the daemon needs. Reads catalogs only.
pub(crate) async fn verify_runtime_role(pool: &PgPool, config: &PgLimits) -> Result<()> {
    let powerful: bool = sqlx::query_scalar(
        "SELECT COALESCE(bool_or(rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls), false) \
         FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .map_err(q_err)?;
    if powerful {
        return Err(role_err("is a superuser or has CREATEROLE/CREATEDB/REPLICATION/BYPASSRLS"));
    }
    let can_create: bool = sqlx::query_scalar("SELECT has_schema_privilege(current_user, 'public', 'CREATE')")
        .fetch_one(pool)
        .await
        .map_err(q_err)?;
    if can_create {
        return Err(role_err("can create objects in the public schema (DDL rights)"));
    }
    let too_much: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') \
           AND (pg_get_userbyid(c.relowner) = current_user OR has_table_privilege(c.oid, 'TRUNCATE') \
                OR has_table_privilege(c.oid, 'TRIGGER') OR has_table_privilege(c.oid, 'REFERENCES'))",
    )
    .fetch_one(pool)
    .await
    .map_err(q_err)?;
    if too_much > 0 {
        return Err(role_err("owns tables or holds TRUNCATE/TRIGGER/REFERENCES on them"));
    }
    let registry_write: bool = sqlx::query_scalar(
        "SELECT COALESCE(bool_or(has_table_privilege(c.oid, 'INSERT') OR has_table_privilege(c.oid, 'UPDATE') \
                                 OR has_table_privilege(c.oid, 'DELETE')), false) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relname IN ('address_owners', '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await
    .map_err(q_err)?;
    if registry_write {
        return Err(role_err("can write the address registry or the migration history"));
    }
    for (name, max) in [
        ("statement_timeout", config.statement_timeout),
        ("idle_in_transaction_session_timeout", config.idle_in_transaction_timeout),
    ] {
        // pg_settings reports these in milliseconds.
        let ms: i64 = sqlx::query_scalar("SELECT setting::bigint FROM pg_settings WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .map_err(q_err)?;
        let max_ms = i64::try_from(max.as_millis()).unwrap_or(i64::MAX);
        if ms <= 0 || ms > max_ms {
            return Err(Error::Invalid(format!(
                "the runtime database role needs a {name} of at most the configured bound (set it with ALTER ROLE ... SET, deploy/db/roles.sql)"
            )));
        }
    }
    Ok(())
}

async fn admin_pool(url: &str, security: &PgSecurity) -> Result<PgPool> {
    let options = connect_options(url, security)?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .map_err(|_| Error::storage("cannot connect to Postgres"))
}

/// Report the schema state of a database (works with the migrator or the runtime role).
pub async fn schema_status(url: &str, security: &PgSecurity) -> Result<SchemaStatus> {
    let pool = admin_pool(url, security).await?;
    let out = schema_status_of(&pool).await;
    pool.close().await;
    out
}

/// Apply every pending migration, then the privilege matrix. Run with the migrator role against
/// Postgres directly (not through a transaction-pooling proxy). Idempotent.
///
/// With `require_roles` a database without the `rusty_app` role is refused before anything
/// changes: a schema without its privilege matrix is not a production database.
pub async fn migrate_postgres(url: &str, security: &PgSecurity, require_roles: bool) -> Result<MigrationReport> {
    let pool = admin_pool(url, security).await?;
    let out = migrate_on(&pool, require_roles).await;
    pool.close().await;
    out
}

async fn migrate_on(pool: &PgPool, require_roles: bool) -> Result<MigrationReport> {
    let may_ddl: bool = sqlx::query_scalar("SELECT has_schema_privilege(current_user, 'public', 'CREATE')")
        .fetch_one(pool)
        .await
        .map_err(q_err)?;
    if !may_ddl {
        return Err(Error::Invalid(
            "this database role cannot change the schema: run db migrate with the migrator role ([store] migrator_user)".into(),
        ));
    }
    let roles_exist: bool = sqlx::query_scalar(
        "SELECT CASE WHEN to_regrole($1) IS NULL OR to_regrole($2) IS NULL THEN false \n         ELSE has_database_privilege($1, current_database(), 'CONNECT') \n              AND NOT has_database_privilege('public', current_database(), 'CONNECT') END",
    )
        .bind(APP_ROLE)
        .bind(RO_ROLE)
        .fetch_one(pool)
        .await
        .map_err(q_err)?;
    if require_roles && !roles_exist {
        return Err(Error::Invalid(
            "the roles rusty_app and rusty_ro are not set up in this database: apply deploy/db/roles.sql first (or pass --without-roles for a throw-away database)"
                .into(),
        ));
    }
    let before = schema_status_of(pool).await?;
    if matches!(before.state, SchemaState::Ahead | SchemaState::Dirty | SchemaState::Mismatch) {
        return Err(Error::storage(
            "database schema history is inconsistent or newer than this binary: refusing to migrate",
        ));
    }
    MIGRATOR.run(pool).await.map_err(|_| Error::storage("Postgres migration failed"))?;
    #[cfg(feature = "mutation-testing")]
    for stmt in crate::mutation::post_migrate(crate::mutation::Dialect::Postgres) {
        sqlx::query(&stmt).execute(pool).await.map_err(|_| Error::storage("mutation statement failed"))?;
    }
    sqlx::raw_sql(GRANTS_SQL).execute(pool).await.map_err(|_| Error::storage("applying the privilege matrix failed"))?;
    let after = schema_status_of(pool).await?;
    Ok(MigrationReport { before, after, grants_applied: roles_exist })
}
