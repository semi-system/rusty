//! The privilege matrix of the service roles (grants_pg.sql): one role per service, each holding what its service does and nothing else.
//!
//! The matrix is checked from the catalogs (`has_table_privilege`, `has_column_privilege`) and by trying forbidden statements as the real roles. The per-service suites
//! (`scripts/pg-role-suites.sh`) show the other direction: that every service still works with what it was given.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const SERVICES: [&str; 5] = ["rusty_smtp_in", "rusty_submission", "rusty_imap", "rusty_outbound", "rusty_gc"];

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
        let may = matches!(role, "rusty_submission" | "rusty_imap");
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
