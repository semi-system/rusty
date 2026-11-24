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

/// The embedded migrations (`crates/rusty-store/migrations`; the numbers 0002..0009 are never used).
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

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

/// Versions of the embedded migrations (for `db status`).
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

/// Whether every migration this binary does not know declares (in `schema_compat`) that a binary at schema version `known` keeps working: the rule that lets the old release run on, and
/// restart on, the schema the new release's migration made (docs/design-zero-downtime.md). A migration with no row, or a row that asks for a newer binary, is not compatible.
async fn newer_schema_is_compatible(pool: &PgPool, known: i64) -> Result<bool> {
    let has_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.schema_compat')::text").fetch_one(pool).await.map_err(q_err)?;
    if has_table.is_none() {
        return Ok(false);
    }
    let blocking: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM _sqlx_migrations m WHERE m.version > $1 
         AND NOT EXISTS (SELECT 1 FROM schema_compat c WHERE c.version = m.version AND c.min_binary <= $1)",
    )
    .bind(known)
    .fetch_one(pool)
    .await
    .map_err(q_err)?;
    Ok(blocking == 0)
}

/// The daemon's start-up check: refuse unless the schema is this binary's, or a newer one whose migrations all keep this binary working.
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
        SchemaState::Ahead => {
            let known = newest(&s.expected);
            // Only a schema that is AHEAD by migrations is a candidate: every migration this binary embeds must be applied and unmodified, or the schema is not the one it was built for.
            let every_known_applied = s.expected.iter().all(|v| s.applied.contains(v));
            if every_known_applied && newer_schema_is_compatible(pool, known).await? {
                tracing::warn!(applied = newest(&s.applied), known, "the schema is newer than this release, and every newer migration keeps it working: running on it (an update is in progress)");
                Ok(())
            } else {
                Err(Error::storage(format!(
                    "database schema is newer than this binary (applied {}, this binary knows {}) and not compatible with it: run a matching or newer release",
                    newest(&s.applied),
                    known
                )))
            }
        }
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

/// How long a statement of the migrator waits for a lock: a DDL statement that waits for a table lock makes every query on the table queue behind it, which is an outage of the mail path
/// however short the DDL itself is. So it gives up after this and the migration is retried (docs/design-zero-downtime.md).
const MIGRATOR_LOCK_TIMEOUT: &str = "5s";
/// Attempts at the migrations (and at the privilege matrix) when a lock timeout ends one, and the pause between them.
const LOCK_RETRIES: u32 = 12;
const LOCK_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_secs(5);

fn is_lock_timeout(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("55P03"))
}

async fn admin_pool(url: &str, security: &PgSecurity) -> Result<PgPool> {
    let options = connect_options(url, security)?;
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SELECT set_config('lock_timeout', $1, false)").bind(MIGRATOR_LOCK_TIMEOUT).execute(conn).await?;
                Ok(())
            })
        })
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
    // Each migration is one transaction and an applied one is skipped, so a run that ended on a lock timeout is simply run again.
    let mut attempt = 0;
    loop {
        match MIGRATOR.run(pool).await {
            Ok(()) => break,
            Err(sqlx::migrate::MigrateError::Execute(e)) if is_lock_timeout(&e) && attempt + 1 < LOCK_RETRIES => {
                attempt += 1;
                tracing::warn!(attempt, "a migration waited too long for a lock (a long transaction is open); trying again");
                tokio::time::sleep(LOCK_RETRY_PAUSE).await;
            }
            Err(_) => return Err(Error::storage("Postgres migration failed")),
        }
    }
    #[cfg(feature = "mutation-testing")]
    for stmt in crate::mutation::post_migrate(crate::mutation::Dialect::Postgres) {
        // Mutation testing only (the feature is never enabled in a release build): the statements come
        // from the checked-in mutation table (src/mutation.rs), not from any runtime input.
        sqlx::query(sqlx::AssertSqlSafe(stmt)).execute(pool).await.map_err(|_| Error::storage("mutation statement failed"))?;
    }
    let mut attempt = 0;
    loop {
        match sqlx::raw_sql(GRANTS_SQL).execute(pool).await {
            Ok(_) => break,
            Err(e) if is_lock_timeout(&e) && attempt + 1 < LOCK_RETRIES => {
                attempt += 1;
                tokio::time::sleep(LOCK_RETRY_PAUSE).await;
            }
            Err(_) => return Err(Error::storage("applying the privilege matrix failed")),
        }
    }
    let after = schema_status_of(pool).await?;
    Ok(MigrationReport { before, after, grants_applied: roles_exist })
}

/// A fingerprint of everything that makes up a migrated schema: the checksums of the embedded migrations (in order) and the privilege matrix. The test database of a suite is built once per
/// fingerprint and then copied (`fixtures::pg_db`), so a change of any migration or grant is a new template, never a stale one.
#[cfg(feature = "test-fixtures")]
pub(crate) fn schema_fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for m in MIGRATOR.iter() {
        h.update(m.version.to_be_bytes());
        h.update(&m.checksum);
    }
    h.update(GRANTS_SQL.as_bytes());
    hex::encode(h.finalize())
}
