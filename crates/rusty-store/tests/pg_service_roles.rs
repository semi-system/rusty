//! The privilege matrix of the service roles (grants_pg.sql): one role per service, each holding what its service does and nothing else.
//!
//! The matrix is checked from the catalogs (`has_table_privilege`, `has_column_privilege`) and by trying forbidden statements as the real roles. The per-service suites
//! (`scripts/pg-role-suites.sh`) show the other direction: that every service still works with what it was given.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const SERVICES: [&str; 6] = ["rusty_smtp_in", "rusty_submission", "rusty_imap", "rusty_outbound", "rusty_gc", "rusty_webmail"];

async fn admin_pool(db: &PgTestDb) -> PgPool {
    PgPoolOptions::new().max_connections(2).connect(db.admin_db_url()).await.unwrap()
}

async fn pool_as(db: &PgTestDb, role: &str) -> PgPool {
    PgPoolOptions::new().max_connections(2).connect(&db.role_url(role)).await.unwrap()
}

async fn table_priv(admin: &PgPool, role: &str, table: &str, privilege: &str) -> bool {
    sqlx::query_scalar("SELECT has_table_privilege($1, $2, $3)").bind(role).bind(table).bind(privilege).fetch_one(admin).await.unwrap()
}

async fn column_priv(admin: &PgPool, role: &str, table: &str, column: &str, privilege: &str) -> bool {
    sqlx::query_scalar("SELECT has_column_privilege($1, $2, $3, $4)").bind(role).bind(table).bind(column).bind(privilege).fetch_one(admin).await.unwrap()
}

async fn tables(admin: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY 1").fetch_all(admin).await.unwrap()
}

fn sqlstate(e: &sqlx::Error) -> String {
    e.as_database_error().and_then(|d| d.code().map(|c| c.into_owned())).unwrap_or_default()
}

/// The statement is refused for lack of privilege (42501).
async fn denied(pool: &PgPool, sql: &str) {
    match sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
        Err(e) => assert_eq!(sqlstate(&e), "42501", "{sql}: {e}"),
        Ok(_) => panic!("{sql} was allowed"),
    }
}

#[tokio::test]
async fn no_service_role_has_ddl_truncate_trigger_or_a_write_on_what_only_the_migrator_or_the_admin_writes() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let admin = admin_pool(&db).await;
    for role in SERVICES {
        let create: bool = sqlx::query_scalar("SELECT has_schema_privilege($1, 'public', 'CREATE')").bind(role).fetch_one(&admin).await.unwrap();
        assert!(!create, "{role} can create objects");
        let temp: bool = sqlx::query_scalar("SELECT has_database_privilege($1, current_database(), 'TEMPORARY')").bind(role).fetch_one(&admin).await.unwrap();
        assert!(!temp, "{role} can create temporary tables");
        for t in tables(&admin).await {
            for p in ["TRUNCATE", "TRIGGER", "REFERENCES"] {
                assert!(!table_priv(&admin, role, &t, p).await, "{role} holds {p} on {t}");
            }
        }
        // The address registry and the migration history are written by the definer triggers and the migrator only; the settings of the tenants and the secrets by the admin web.
        for t in ["address_owners", "_sqlx_migrations", "sealed_secrets", "admin_grants", "plans", "domain_dns_config"] {
            for p in ["INSERT", "UPDATE", "DELETE"] {
                assert!(!table_priv(&admin, role, t, p).await, "{role} can {p} {t}");
            }
        }
    }
}

#[tokio::test]
async fn only_the_services_that_check_a_password_can_read_the_hashes() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let admin = admin_pool(&db).await;
    for role in SERVICES {
        let may = matches!(role, "rusty_submission" | "rusty_imap" | "rusty_webmail");
        assert_eq!(column_priv(&admin, role, "accounts", "password_hash", "SELECT").await, may, "{role} and accounts.password_hash");
        assert!(!column_priv(&admin, role, "accounts", "password_hash", "UPDATE").await, "{role} can change a password hash");
        // `password_set` is what the others read to know that an account has a password.
        assert!(column_priv(&admin, role, "accounts", "password_set", "SELECT").await, "{role} cannot read accounts.password_set");
    }
    let smtp_in = pool_as(&db, "rusty_smtp_in").await;
    denied(&smtp_in, "SELECT password_hash FROM accounts").await;
    denied(&smtp_in, "SELECT count(*) FROM accounts WHERE password_hash IS NULL").await;
}

#[tokio::test]
async fn each_service_keeps_to_its_own_writes() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let admin = admin_pool(&db).await;
    // (role, table, privilege, expected)
    let cases = [
        // only the collector deletes message bodies' references and blobs; no one but the delivering services creates a message
        ("rusty_gc", "blobs", "DELETE", true),
        // the removal of a closed account after its retention is the collector's alone (docs/design-delete.md)
        ("rusty_gc", "accounts", "DELETE", true),
        ("rusty_imap", "accounts", "DELETE", false),
        ("rusty_smtp_in", "accounts", "DELETE", false),
        ("rusty_submission", "accounts", "DELETE", false),
        ("rusty_outbound", "accounts", "DELETE", false),
        ("rusty_smtp_in", "blobs", "DELETE", false),
        ("rusty_submission", "blobs", "DELETE", false),
        ("rusty_imap", "blobs", "DELETE", false),
        ("rusty_outbound", "blobs", "DELETE", false),
        ("rusty_gc", "messages", "INSERT", false),
        ("rusty_smtp_in", "messages", "INSERT", true),
        ("rusty_submission", "messages", "INSERT", true),
        ("rusty_imap", "messages", "INSERT", true),
        ("rusty_outbound", "messages", "INSERT", true),
        // expunge belongs to the mailbox access; delivery never deletes
        ("rusty_imap", "messages", "DELETE", true),
        ("rusty_webmail", "messages", "DELETE", true),
        ("rusty_webmail", "blobs", "DELETE", false),
        ("rusty_webmail", "jobs", "INSERT", false),
        // app passwords: made and revoked by the webmail, read (and noted as used) by the services that check a login, never by the others
        ("rusty_webmail", "app_passwords", "DELETE", true),
        ("rusty_webmail", "webmail_settings", "UPDATE", true),
        ("rusty_webmail", "totp", "UPDATE", true),
        ("rusty_imap", "totp", "UPDATE", false),
        ("rusty_submission", "totp_recovery", "SELECT", false),
        ("rusty_gc", "totp", "SELECT", false),
        ("rusty_imap", "webmail_settings", "SELECT", false),
        ("rusty_imap", "app_passwords", "SELECT", true),
        ("rusty_submission", "app_passwords", "SELECT", true),
        ("rusty_imap", "app_passwords", "INSERT", false),
        ("rusty_submission", "app_passwords", "DELETE", false),
        ("rusty_smtp_in", "app_passwords", "SELECT", false),
        ("rusty_gc", "app_passwords", "SELECT", false),
        ("rusty_smtp_in", "messages", "DELETE", false),
        ("rusty_submission", "messages", "DELETE", false),
        ("rusty_outbound", "messages", "DELETE", false),
        // the outbound queue
        ("rusty_outbound", "jobs", "DELETE", true),
        ("rusty_smtp_in", "jobs", "INSERT", true),
        ("rusty_submission", "jobs", "INSERT", true),
        ("rusty_imap", "jobs", "INSERT", false),
        ("rusty_imap", "jobs", "SELECT", false),
        // DKIM keys and relays are read by the delivery worker and written by the admin web
        ("rusty_outbound", "domain_dkim_keys", "SELECT", true),
        ("rusty_outbound", "domain_dkim_keys", "INSERT", false),
        ("rusty_imap", "domain_dkim_keys", "SELECT", false),
        ("rusty_smtp_in", "domain_relays", "SELECT", false),
        // the domain and account directory is read, never written, by any service
        ("rusty_smtp_in", "domains", "INSERT", false),
        ("rusty_imap", "accounts", "INSERT", false),
        ("rusty_gc", "aliases", "SELECT", false),
        // heartbeats: written only through rs_heartbeat(); the collector alone deletes the stale ones
        ("rusty_gc", "service_heartbeats", "DELETE", true),
        ("rusty_imap", "service_heartbeats", "DELETE", false),
        ("rusty_imap", "service_heartbeats", "INSERT", false),
        ("rusty_smtp_in", "service_heartbeats", "SELECT", false),
        // the periodic DNS check of the collector asks which domains are due and locks the domain row it queues a check for
        ("rusty_gc", "domain_dns_state", "SELECT", true),
        ("rusty_gc", "domain_dns_state", "INSERT", false),
        // the collector's leases
        ("rusty_gc", "blob_store_leases", "INSERT", true),
        ("rusty_imap", "blob_store_leases", "SELECT", false),
    ];
    for (role, table, privilege, expected) in cases {
        assert_eq!(table_priv(&admin, role, table, privilege).await, expected, "{role}: {privilege} on {table}");
    }
}

#[tokio::test]
async fn forbidden_statements_are_refused_by_the_real_roles() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let imap = pool_as(&db, "rusty_imap").await;
    denied(&imap, "DELETE FROM blobs").await;
    denied(&imap, "INSERT INTO domains (name, created_at) VALUES ('evil.test', 0)").await;
    denied(&imap, "SELECT * FROM jobs").await;
    denied(&imap, "UPDATE sealed_secrets SET sealed = ''").await;
    let gc = pool_as(&db, "rusty_gc").await;
    denied(&gc, "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date) VALUES (1, 1, 1, 1, 'x', 1, 0)").await;
    denied(&gc, "SELECT * FROM aliases").await;
    let outbound = pool_as(&db, "rusty_outbound").await;
    denied(&outbound, "INSERT INTO domain_dkim_keys (domain_id, selector, algorithm, public_key_b64) VALUES (1, 's', 'ed25519', 'k')").await;
    denied(&outbound, "UPDATE accounts SET status = 'locked'").await;
    let smtp_in = pool_as(&db, "rusty_smtp_in").await;
    denied(&smtp_in, "DELETE FROM messages").await;
    denied(&smtp_in, "SELECT * FROM domain_relays").await;
}

#[tokio::test]
async fn every_service_role_may_send_its_heartbeat_and_the_reporting_role_may_not() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let admin = admin_pool(&db).await;
    for role in SERVICES.into_iter().chain(["rusty_admin", "rusty_app"]) {
        let may: bool = sqlx::query_scalar("SELECT has_function_privilege($1, 'rs_heartbeat(text, text, text, bigint, text, text)', 'EXECUTE')").bind(role).fetch_one(&admin).await.unwrap();
        assert!(may, "{role} cannot send a heartbeat");
    }
    let may: bool = sqlx::query_scalar("SELECT has_function_privilege('rusty_ro', 'rs_heartbeat(text, text, text, bigint, text, text)', 'EXECUTE')").fetch_one(&admin).await.unwrap();
    assert!(!may, "the reporting role writes nothing");
    let public: bool = sqlx::query_scalar("SELECT has_function_privilege('public', 'rs_heartbeat(text, text, text, bigint, text, text)', 'EXECUTE')").fetch_one(&admin).await.unwrap();
    assert!(!public, "the function is not open to PUBLIC");
}

/// The storage check worker runs in the delivery worker (`rusty_outbound`): it locks a store or a backend row and records the result of a check. Without the privilege the check never completes, the store of a
/// domain stays `unchecked`, and a store is used only after a check passed: every message for that domain was refused with a temporary error (found on the swarm test server, 2026-11-17).
#[tokio::test]
async fn the_storage_check_worker_can_record_a_check_and_nothing_else_on_the_stores() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let admin = admin_pool(&db).await;
    sqlx::query("INSERT INTO domains (name, created_at) VALUES ('t.test', 0)").execute(&admin).await.unwrap();
    sqlx::query("INSERT INTO blob_stores (domain_id, kind, created_at) SELECT id, 's3', 0 FROM domains WHERE name = 't.test'").execute(&admin).await.unwrap();
    sqlx::query("INSERT INTO blob_backends (store_id, name, kind, created_at) SELECT id, 'b1', 's3', 0 FROM blob_stores WHERE domain_id IS NOT NULL").execute(&admin).await.unwrap();
    let outbound = pool_as(&db, "rusty_outbound").await;
    // the statements of the worker (repo/blob_pool.rs and repo/pg/blob_store.rs)
    sqlx::query("SELECT id FROM blob_stores WHERE domain_id IS NOT NULL FOR UPDATE").fetch_all(&outbound).await.unwrap();
    sqlx::query("SELECT id FROM blob_backends FOR UPDATE").fetch_all(&outbound).await.unwrap();
    sqlx::query("UPDATE blob_stores SET check_status = 'ok', checked_at = 1 WHERE domain_id IS NOT NULL").execute(&outbound).await.unwrap();
    sqlx::query("UPDATE blob_backends SET check_status = 'ok', checked_at = 1").execute(&outbound).await.unwrap();
    // it records a result; it does not change what a store is
    denied(&outbound, "UPDATE blob_backends SET config = '{}'::jsonb").await;
    denied(&outbound, "UPDATE blob_backends SET state = 'disabled'").await;
    denied(&outbound, "UPDATE blob_stores SET config = '{}'::jsonb").await;
    denied(&outbound, "INSERT INTO blob_backends (store_id, name, kind, created_at) VALUES (1, 'x', 's3', 0)").await;
    denied(&outbound, "DELETE FROM blob_backends").await;
    // and no other delivering service records checks
    for role in ["rusty_smtp_in", "rusty_submission", "rusty_imap"] {
        let p = pool_as(&db, role).await;
        denied(&p, "UPDATE blob_backends SET check_status = 'ok'").await;
        denied(&p, "UPDATE blob_stores SET check_status = 'ok'").await;
    }
}
