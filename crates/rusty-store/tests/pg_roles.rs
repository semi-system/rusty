#![cfg(feature = "postgres")]
//! Roles, privileges, the schema-version check and the explicit migration step (U11, PG8).
//!
//! Every test creates its own database, initialises it with deploy/db/roles.sql (the file the
//! operator runs) and connects as the real roles, never as the superuser, except where the
//! superuser is the thing under test. See docs/postgres-production.md for the threat model.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::config::SslMode;
use rusty_core::Error;
use rusty_store::repo::pg::PgRepo;
use rusty_store::repo::{DirectoryRepo, NewAccount};
use rusty_store::{
    migrate_postgres, schema_status, PgPoolConfig, PgSecurity, PgStore, SchemaState, Store, StoreConfig,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn pool_as(db: &PgTestDb, role: &str) -> PgPool {
    PgPoolOptions::new().max_connections(2).connect(&db.role_url(role)).await.unwrap()
}

async fn admin_pool(db: &PgTestDb) -> PgPool {
    PgPoolOptions::new().max_connections(2).connect(db.admin_db_url()).await.unwrap()
}

fn sqlstate(e: &sqlx::Error) -> String {
    e.as_database_error().and_then(|d| d.code().map(|c| c.into_owned())).unwrap_or_default()
}

/// The statement is refused for lack of privilege (42501 insufficient_privilege).
async fn denied(pool: &PgPool, sql: &str) {
    let r = sqlx::query(sql).execute(pool).await;
    match r {
        Err(e) => assert_eq!(sqlstate(&e), "42501", "{sql}: {e}"),
        Ok(_) => panic!("{sql} was allowed"),
    }
}

async fn runtime_repo(db: &PgTestDb) -> PgRepo {
    PgRepo::new(PgStore::connect(&db.role_url("rusty_app")).await.unwrap())
}

// backend-neutral: roles and privileges exist only in Postgres; SQLite has no users
#[tokio::test]
async fn runtime_role_has_no_ddl_truncate_or_registry_write() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let app = pool_as(&db, "rusty_app").await;
    for sql in [
        "CREATE TABLE evil (id int)",
        "CREATE SCHEMA evil",
        "CREATE TEMP TABLE address_owners (id int)",
        "TRUNCATE accounts CASCADE",
        "TRUNCATE address_owners",
        "TRUNCATE lists CASCADE",
        "INSERT INTO address_owners (domain_id, local_part, owner_kind, owner_id) VALUES (1, 'x', 'account', 1)",
        "UPDATE address_owners SET local_part = 'y'",
        "DELETE FROM address_owners",
        "ALTER TABLE accounts ADD COLUMN evil int",
        "DROP TABLE accounts CASCADE",
        "ALTER FUNCTION rs_address_unique() SECURITY INVOKER",
        "CREATE OR REPLACE FUNCTION rs_now() RETURNS bigint LANGUAGE sql AS 'SELECT 1::bigint'",
        "DELETE FROM _sqlx_migrations",
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (99, 'x', true, '\\x00', 0)",
        "CREATE TRIGGER evil BEFORE INSERT ON accounts FOR EACH ROW EXECUTE FUNCTION rs_now()",
    ] {
        denied(&app, sql).await;
    }
    app.close().await;
    db.cleanup().await;
}

// backend-neutral: roles and privileges exist only in Postgres; SQLite has no users
#[tokio::test]
async fn runtime_role_still_maintains_the_registry_through_the_definer_triggers() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let repo = runtime_repo(&db).await;
    let dom = repo.create_domain("roles.test").await.unwrap();
    let acc = repo
        .create_account(&NewAccount { local: "alice".into(), domain: dom, plan: None, password_hash: None })
        .await
        .unwrap();
    // The registry row exists (written by the SECURITY DEFINER trigger, not by the role).
    let app = pool_as(&db, "rusty_app").await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM address_owners WHERE owner_kind = 'account' AND owner_id = $1")
        .bind(acc.0)
        .fetch_one(&app)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // A second account with the same address is a conflict, enforced through the registry.
    let dup = repo
        .create_account(&NewAccount { local: "alice".into(), domain: dom, plan: None, password_hash: None })
        .await;
    assert!(matches!(dup, Err(Error::Conflict(_))), "{dup:?}");
    // Deleting through the table keeps the registry in step (the DELETE trigger runs as definer).
    sqlx::query("DELETE FROM accounts WHERE id = $1").bind(acc.0).execute(&app).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM address_owners WHERE owner_kind = 'account' AND owner_id = $1")
        .bind(acc.0)
        .fetch_one(&app)
        .await
        .unwrap();
    assert_eq!(n, 0);
    app.close().await;
    db.cleanup().await;
}

// backend-neutral: roles and privileges exist only in Postgres; SQLite has no users
#[tokio::test]
async fn address_registry_functions_are_security_definer_with_a_pinned_search_path() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let a = admin_pool(&db).await;
    let rows: Vec<(String, bool, Option<Vec<String>>, String)> = sqlx::query_as(
        "SELECT p.proname::text, p.prosecdef, p.proconfig, pg_get_userbyid(p.proowner)::text \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.proname IN ('rs_address_unique', 'rs_address_truncate') ORDER BY 1",
    )
    .fetch_all(&a)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for (name, definer, config, owner) in rows {
        assert!(definer, "{name} must be SECURITY DEFINER");
        assert_eq!(owner, "rusty_migrator", "{name} must be owned by the migrator");
        let cfg = config.unwrap_or_default().join(",");
        assert!(cfg.contains("search_path=pg_catalog, public, pg_temp"), "{name}: {cfg}");
    }
    // Every other trigger function stays SECURITY INVOKER with a pinned search_path.
    let others: Vec<(String,)> = sqlx::query_as(
        "SELECT p.proname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.prosecdef AND p.proname NOT IN ('rs_address_unique', 'rs_address_truncate')",
    )
    .fetch_all(&a)
    .await
    .unwrap();
    assert!(others.is_empty(), "unexpected SECURITY DEFINER functions: {others:?}");
    a.close().await;
    db.cleanup().await;
}

// backend-neutral: roles and privileges exist only in Postgres; SQLite has no users
#[tokio::test]
async fn read_only_role_cannot_write_or_read_password_hashes() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let repo = runtime_repo(&db).await;
    let dom = repo.create_domain("ro.test").await.unwrap();
    repo.create_account(&NewAccount { local: "bob".into(), domain: dom, plan: None, password_hash: Some("$argon2id$secret".into()) })
        .await
        .unwrap();
    let ro = pool_as(&db, "rusty_ro").await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts").fetch_one(&ro).await.unwrap();
    assert_eq!(n, 1);
    let local: String = sqlx::query_scalar("SELECT local_part FROM accounts").fetch_one(&ro).await.unwrap();
    assert_eq!(local, "bob");
    denied(&ro, "SELECT password_hash FROM accounts").await;
    denied(&ro, "SELECT * FROM accounts").await;
    denied(&ro, "INSERT INTO domains (name) VALUES ('x.test')").await;
    denied(&ro, "DELETE FROM mailboxes").await;
    denied(&ro, "TRUNCATE domains CASCADE").await;
    denied(&ro, "CREATE TABLE t (id int)").await;
    ro.close().await;
    db.cleanup().await;
}

// backend-neutral: roles and privileges exist only in Postgres; SQLite has no users
#[tokio::test]
async fn roles_are_plain_roles_and_the_database_is_closed_to_public() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let a = admin_pool(&db).await;
    // Nobody but the three roles (and superusers) may connect or create temporary tables.
    let public_connect: bool = sqlx::query_scalar("SELECT has_database_privilege('public', current_database(), 'CONNECT')")
        .fetch_one(&a)
        .await
        .unwrap();
    assert!(!public_connect);
    let public_temp: bool = sqlx::query_scalar("SELECT has_database_privilege('public', current_database(), 'TEMP')")
        .fetch_one(&a)
        .await
        .unwrap();
    assert!(!public_temp);
    for role in ["rusty_app", "rusty_ro", "rusty_migrator"] {
        let (super_, createrole, createdb, repl, bypass): (bool, bool, bool, bool, bool) = sqlx::query_as(
            "SELECT rolsuper, rolcreaterole, rolcreatedb, rolreplication, rolbypassrls FROM pg_roles WHERE rolname = $1",
        )
        .bind(role)
        .fetch_one(&a)
        .await
        .unwrap();
        assert!(!(super_ || createrole || createdb || repl || bypass), "{role} must be a plain role");
    }
    a.close().await;
    db.cleanup().await;
}

// backend-neutral: role-level settings and the schema history table exist only in Postgres
#[tokio::test]
async fn statement_and_idle_timeouts_are_role_level_not_session_level() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let app = pool_as(&db, "rusty_app").await;
    let (st, idle): (String, String) =
        sqlx::query_as("SELECT current_setting('statement_timeout'), current_setting('idle_in_transaction_session_timeout')")
            .fetch_one(&app)
            .await
            .unwrap();
    assert_eq!((st.as_str(), idle.as_str()), ("30s", "30s"));
    // connect() itself sets nothing: a superuser (no role-level setting) sees the server default.
    let store = PgStore::connect(db.admin_db_url()).await.unwrap();
    let st: String = sqlx::query_scalar("SHOW statement_timeout").fetch_one(store.pool()).await.unwrap();
    assert_eq!(st, "0");
    let idle: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout").fetch_one(store.pool()).await.unwrap();
    assert_eq!(idle, "0");
    store.close().await;
    app.close().await;
    db.cleanup().await;
}

fn strict() -> PgSecurity {
    PgSecurity { verify_role: true, ..PgSecurity::default() }
}

fn invalid_role<T: std::fmt::Debug>(r: rusty_core::Result<T>, needle: &str) {
    match r {
        Err(Error::Invalid(m)) => assert!(m.contains(needle), "{m}"),
        other => panic!("expected Invalid({needle}), got {other:?}"),
    }
}

// backend-neutral: roles and privileges exist only in Postgres; SQLite has no users
#[tokio::test]
async fn startup_refuses_a_runtime_role_with_ddl_rights_or_without_timeouts() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let cfg = PgPoolConfig::default();

    // The proper runtime role passes.
    PgStore::connect_with(&db.role_url("rusty_app"), cfg.clone(), &strict()).await.unwrap().close().await;
    // A superuser, the migrator (owns the tables, can create objects) and the read-only role's
    // missing DML are refused or accepted for the right reasons.
    invalid_role(PgStore::connect_with(db.admin_db_url(), cfg.clone(), &strict()).await, "superuser");
    invalid_role(PgStore::connect_with(&db.role_url("rusty_migrator"), cfg.clone(), &strict()).await, "DDL");

    // A role-level timeout of 0 (disabled) or one above the configured bound is refused.
    let a = admin_pool(&db).await;
    let dbname = db.name().to_owned();
    for (value, why) in [("0", "disabled"), ("2h", "too long")] {
        sqlx::query(&format!("ALTER ROLE rusty_app IN DATABASE \"{dbname}\" SET statement_timeout = '{value}'"))
            .execute(&a)
            .await
            .unwrap();
        invalid_role(PgStore::connect_with(&db.role_url("rusty_app"), cfg.clone(), &strict()).await, "statement_timeout");
        let _ = why;
    }
    sqlx::query(&format!("ALTER ROLE rusty_app IN DATABASE \"{dbname}\" RESET statement_timeout")).execute(&a).await.unwrap();
    sqlx::query(&format!("ALTER ROLE rusty_app IN DATABASE \"{dbname}\" SET idle_in_transaction_session_timeout = 0"))
        .execute(&a)
        .await
        .unwrap();
    invalid_role(
        PgStore::connect_with(&db.role_url("rusty_app"), cfg.clone(), &strict()).await,
        "idle_in_transaction_session_timeout",
    );
    sqlx::query(&format!("ALTER ROLE rusty_app IN DATABASE \"{dbname}\" RESET idle_in_transaction_session_timeout"))
        .execute(&a)
        .await
        .unwrap();
    PgStore::connect_with(&db.role_url("rusty_app"), cfg.clone(), &strict()).await.unwrap().close().await;

    // Extra rights granted by mistake are caught too.
    for (grant, revoke) in [
        ("GRANT TRUNCATE ON accounts TO rusty_app", "REVOKE TRUNCATE ON accounts FROM rusty_app"),
        ("GRANT INSERT ON address_owners TO rusty_app", "REVOKE INSERT ON address_owners FROM rusty_app"),
        ("GRANT UPDATE ON _sqlx_migrations TO rusty_app", "REVOKE UPDATE ON _sqlx_migrations FROM rusty_app"),
        ("GRANT CREATE ON SCHEMA public TO rusty_app", "REVOKE CREATE ON SCHEMA public FROM rusty_app"),
    ] {
        sqlx::query(grant).execute(&a).await.unwrap();
        let r = PgStore::connect_with(&db.role_url("rusty_app"), cfg.clone(), &strict()).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{grant}: {r:?}");
        sqlx::query(revoke).execute(&a).await.unwrap();
    }
    a.close().await;
    db.cleanup().await;
}

// backend-neutral: the schema history table and the explicit migrator exist only in Postgres
#[tokio::test]
async fn connect_never_migrates_and_refuses_an_unmigrated_database() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_unmigrated(&admin).await;
    let err = PgStore::connect(db.url()).await.unwrap_err();
    match &err {
        Error::Storage { message, .. } => {
            assert!(message.contains("not initialised") && message.contains("rustyctl db migrate"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // Store::open goes through the same check.
    let err = Store::open(StoreConfig::postgres(db.url())).await.err().unwrap();
    assert!(err.to_string().contains("not initialised"), "{err}");
    // Nothing was created by the failed attempts.
    let a = admin_pool(&db).await;
    let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'").fetch_one(&a).await.unwrap();
    assert_eq!(tables, 0);
    a.close().await;
    db.cleanup().await;
}

// backend-neutral: the schema history table and the explicit migrator exist only in Postgres
#[tokio::test]
async fn connect_refuses_every_schema_version_mismatch_with_a_clear_error() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let a = admin_pool(&db).await;
    let newest: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&a).await.unwrap();
    assert!(newest >= 16);
    let msg = |e: Error| match e {
        Error::Storage { message, .. } => message,
        other => panic!("{other:?}"),
    };

    // Behind: the newest migration is not applied.
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1").bind(newest).execute(&a).await.unwrap();
    let m = msg(PgStore::connect(db.url()).await.unwrap_err());
    assert!(m.contains("older than this binary") && m.contains("rustyctl db migrate"), "{m}");
    let s = schema_status(db.url(), &PgSecurity::default()).await.unwrap();
    assert_eq!(s.state, SchemaState::Behind);
    assert_eq!(s.pending(), vec![newest]);
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         SELECT $1, 'restored', true, checksum, 0 FROM (SELECT '\\x00'::bytea AS checksum) t",
    )
    .bind(newest)
    .execute(&a)
    .await
    .unwrap();
    // The restored row has a wrong checksum: Mismatch (an applied migration differs from the embedded one).
    let m = msg(PgStore::connect(db.url()).await.unwrap_err());
    assert!(m.contains("inconsistent"), "{m}");
    assert_eq!(schema_status(db.url(), &PgSecurity::default()).await.unwrap().state, SchemaState::Mismatch);
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1").bind(newest).execute(&a).await.unwrap();
    a.close().await;
    db.cleanup().await;
}

// backend-neutral: the schema history table and the explicit migrator exist only in Postgres
#[tokio::test]
async fn connect_refuses_a_database_migrated_by_a_newer_binary_or_with_a_failed_migration() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create(&admin).await;
    let a = admin_pool(&db).await;
    let msg = |e: Error| match e {
        Error::Storage { message, .. } => message,
        other => panic!("{other:?}"),
    };
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (9999, 'future', true, '\\x00', 0)")
        .execute(&a)
        .await
        .unwrap();
    let m = msg(PgStore::connect(db.url()).await.unwrap_err());
    assert!(m.contains("newer than this binary"), "{m}");
    assert_eq!(schema_status(db.url(), &PgSecurity::default()).await.unwrap().state, SchemaState::Ahead);
    // The migrator refuses to touch such a database as well.
    let r = migrate_postgres(db.admin_db_url(), &PgSecurity::default(), false).await;
    assert!(matches!(r, Err(Error::Storage { .. })), "{r:?}");
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9999").execute(&a).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = (SELECT max(version) FROM _sqlx_migrations)")
        .execute(&a)
        .await
        .unwrap();
    let m = msg(PgStore::connect(db.url()).await.unwrap_err());
    assert!(m.contains("inconsistent"), "{m}");
    assert_eq!(schema_status(db.url(), &PgSecurity::default()).await.unwrap().state, SchemaState::Dirty);
    a.close().await;
    db.cleanup().await;
}

// backend-neutral: the explicit migrator and its roles exist only in Postgres
#[tokio::test]
async fn migrate_is_explicit_idempotent_and_applies_the_privilege_matrix() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_unmigrated(&admin).await;
    // Without the roles a production migrate is refused before anything changes.
    let r = migrate_postgres(db.admin_db_url(), &PgSecurity::default(), true).await;
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    assert_eq!(schema_status(db.admin_db_url(), &PgSecurity::default()).await.unwrap().state, SchemaState::Uninitialised);
    db.cleanup().await;

    let db = PgTestDb::create_with_roles(&admin).await; // roles + migrated once by the migrator
    let url = db.role_url("rusty_migrator");
    let again = migrate_postgres(&url, &PgSecurity::default(), true).await.unwrap();
    assert_eq!(again.before.state, SchemaState::Current);
    assert_eq!(again.after.state, SchemaState::Current);
    assert_eq!(again.before.applied, again.after.applied);
    assert!(again.grants_applied);
    assert!(again.after.pending().is_empty());
    // The runtime role cannot migrate.
    let r = migrate_postgres(&db.role_url("rusty_app"), &PgSecurity::default(), true).await;
    assert!(r.is_err(), "the runtime role must not be able to migrate");
    db.cleanup().await;
}

// backend-neutral: TLS to the database applies to Postgres only
#[tokio::test]
async fn tls_policy_refuses_unverified_remote_hosts_without_connecting() {
    // None of these hosts is contacted: the policy runs before any socket is opened.
    for url in [
        "postgres://rusty_app@db.example.test/rusty?sslmode=disable",
        "postgres://rusty_app@db.example.test/rusty?sslmode=require",
        "postgres://rusty_app@db.example.test/rusty?sslmode=verify-ca",
        "postgres://rusty_app@db.example.test/rusty",
        "postgres://rusty_app@10.20.30.40/rusty?sslmode=prefer",
    ] {
        let r = PgStore::connect(url).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{url}: {r:?}");
    }
    // The explicit key overrides the URL; verify-full passes the policy (and then fails to connect: the host does not exist).
    let sec = PgSecurity { ssl_mode: Some(SslMode::Disable), ..PgSecurity::default() };
    let r = PgStore::connect_with("postgres://u@db.example.test/d?sslmode=verify-full", PgPoolConfig::default(), &sec).await;
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    let sec = PgSecurity { ssl_mode: Some(SslMode::VerifyFull), ..PgSecurity::default() };
    let r = PgStore::connect_with("postgres://u@nonexistent.invalid/d?sslmode=disable", PgPoolConfig::default(), &sec).await;
    assert!(matches!(r, Err(Error::Storage { .. })), "{r:?}");
    // The explicit insecure override lets an unverified remote host through the policy.
    let sec = PgSecurity { allow_unverified_tls: true, ..PgSecurity::default() };
    let r = PgStore::connect_with("postgres://u@nonexistent.invalid/d?sslmode=disable", PgPoolConfig::default(), &sec).await;
    assert!(matches!(r, Err(Error::Storage { .. })), "{r:?}");
    // Loopback needs no TLS for the policy (the test servers listen there).
    let r = PgStore::connect("postgres://u@127.0.0.1:1/d?sslmode=disable").await;
    assert!(matches!(r, Err(Error::Storage { .. })), "{r:?}");
}

// backend-neutral: connection-string handling of the Postgres driver; SQLite has no credentials
#[tokio::test]
async fn password_comes_from_the_security_struct_and_never_shows_in_errors_or_debug() {
    let Some(admin) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&admin).await;
    let secret = "wrong-password-do-not-leak";
    let sec = PgSecurity {
        password: Some(rusty_store::PgPassword::new(secret.into())),
        ..PgSecurity::default()
    };
    assert!(!format!("{sec:?}").contains(secret));
    let url = db.role_url("rusty_app");
    let no_pw = url.replace("rusty-test-app", "x");
    let err = PgStore::connect_with(&no_pw, PgPoolConfig::default(), &sec).await.unwrap_err();
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains(secret) && !shown.contains("rusty-test-app"), "{shown}");
    let cfg = StoreConfig::Postgres { url: no_pw.clone(), pool: PgPoolConfig::default(), security: sec };
    assert!(!format!("{cfg:?}").contains(secret));
    db.cleanup().await;
}
