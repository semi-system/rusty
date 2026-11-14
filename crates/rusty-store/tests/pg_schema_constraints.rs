
mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::{BlobHash, Error};
use rusty_store::{map_db_error, PgStore};
use sqlx::PgPool;

struct Seed {
    domain: i64,
    account_a: i64,
}

async fn database() -> Option<(PgTestDb, PgStore)> {
    let url = pg_admin_url()?;
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    Some((db, store))
}

async fn seed(pool: &PgPool) -> Seed {
    let domain: i64 = sqlx::query_scalar(
        "INSERT INTO domains(name,created_at) VALUES ('example.com',0) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let account_a: i64 = sqlx::query_scalar(
        "INSERT INTO accounts(local_part,domain_id,status,created_at) \
         VALUES ('a',$1,'active',0) RETURNING id",
    )
    .bind(domain)
    .fetch_one(pool)
    .await
    .unwrap();
    let account_b: i64 = sqlx::query_scalar(
        "INSERT INTO accounts(local_part,domain_id,status,created_at) \
         VALUES ('b',$1,'active',0) RETURNING id",
    )
    .bind(domain)
    .fetch_one(pool)
    .await
    .unwrap();
    let mailbox_a: i64 = sqlx::query_scalar(
        "INSERT INTO mailboxes(account_id,uid_validity) VALUES ($1,1) RETURNING id",
    )
    .bind(account_a)
    .fetch_one(pool)
    .await
    .unwrap();
    let mailbox_b: i64 = sqlx::query_scalar(
        "INSERT INTO mailboxes(account_id,uid_validity) VALUES ($1,1) RETURNING id",
    )
    .bind(account_b)
    .fetch_one(pool)
    .await
    .unwrap();
    for mailbox in [mailbox_a, mailbox_b] {
        sqlx::query(
            "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
             VALUES ($1,1,NULL,'Inbox','inbox',1)",
        )
        .bind(mailbox)
        .execute(pool)
        .await
        .unwrap();
    }
    for byte in [0xaau8, 0xbbu8] {
        sqlx::query("INSERT INTO blobs(hash,size) VALUES ($1,10)")
            .bind(vec![byte; 32])
            .execute(pool)
            .await
            .unwrap();
    }
    Seed {
        domain,
        account_a,
    }
}

#[tokio::test]
async fn pg_bytea_blob_hash_round_trips_without_changing_public_hex_type() {
    let Some((db, store)) = database().await else {
        return;
    };
    let hash = BlobHash::of(b"postgres bytea mapping");
    let bytes = hex::decode(hash.as_str()).unwrap();
    sqlx::query("INSERT INTO blobs(hash,size) VALUES ($1,1)")
        .bind(&bytes)
        .execute(store.pool())
        .await
        .unwrap();
    let fetched: Vec<u8> = sqlx::query_scalar("SELECT hash FROM blobs WHERE hash=$1")
        .bind(&bytes)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(BlobHash::parse(&hex::encode(fetched)).unwrap(), hash);
    store.close().await;
    db.cleanup().await;
}

async fn bad(pool: &PgPool, sql: &str) -> sqlx::Error {
    sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await.unwrap_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_address_uniqueness_is_concurrency_safe() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    for n in 0..12 {
        let local = format!("race{n}");
        let q1 =
            sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ($1,$2,$3)")
                .bind(&local)
                .bind(s.domain)
                .bind(s.account_a)
                .execute(p);
        let q2 = sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ($1,$2)")
            .bind(&local)
            .bind(s.domain)
            .execute(p);
        let (a, b) = tokio::join!(q1, q2);
        assert_ne!(a.is_ok(), b.is_ok(), "both results matched for {local}");
        let owners: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM aliases WHERE local_part=$1 AND domain_id=$2) + \
                    (SELECT count(*) FROM lists WHERE local_part=$1 AND domain_id=$2)",
        )
        .bind(&local)
        .bind(s.domain)
        .fetch_one(p)
        .await
        .unwrap();
        assert_eq!(owners, 1);
    }
    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_error_mapping_uses_sqlstate_not_message_text() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    let check = bad(
        p,
        "INSERT INTO domains(name,created_at) VALUES ('RS001-UPPER',0)",
    )
    .await;
    assert!(matches!(map_db_error(check), Error::Invalid(_)));

    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('used',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    let custom = sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('used',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .unwrap_err();
    assert_eq!(
        custom.as_database_error().unwrap().code().as_deref(),
        Some("RS001")
    );
    assert!(matches!(map_db_error(custom), Error::Conflict(_)));

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_dropping_address_trigger_demonstrates_negative_test_sensitivity() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('mutant',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER lists_addr_unique_ins ON lists")
        .execute(p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('mutant',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .expect("the forbidden state must become possible after the guarding trigger is removed");
    store.close().await;
    db.cleanup().await;
}

// The production error mapping (`map_db_error`) classifies Postgres errors by SQLSTATE only and
// answers with fixed texts.
#[tokio::test]
async fn pg_map_db_error_classifies_constraint_violations_by_sqlstate() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    let code_of = |e: &sqlx::Error| {
        e.as_database_error()
            .and_then(|d| d.code().map(|c| c.to_string()))
            .unwrap_or_default()
    };
    // unique violation -> Conflict("already exists")
    let e = bad(p, "INSERT INTO domains(name,created_at) VALUES ('example.com',0)").await;
    assert_eq!(code_of(&e), "23505");
    assert!(matches!(map_db_error(e), Error::Conflict(m) if m == "already exists"));
    // foreign key violation -> Conflict(referenced ...)
    let e = bad(
        p,
        "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('z',999999,'active',0)",
    )
    .await;
    assert_eq!(code_of(&e), "23503");
    assert!(matches!(map_db_error(e), Error::Conflict(m) if m == "referenced row missing or still in use"));
    // restrict violation (delete of a referenced domain) -> Conflict(referenced ...)
    let e = bad(p, &format!("DELETE FROM domains WHERE id={}", s.domain)).await;
    assert_eq!(code_of(&e), "23001");
    assert!(matches!(map_db_error(e), Error::Conflict(m) if m == "referenced row missing or still in use"));
    // check violation -> Invalid
    let e = bad(p, "INSERT INTO domains(name,created_at) VALUES ('UPPER.example',0)").await;
    assert_eq!(code_of(&e), "23514");
    assert!(matches!(map_db_error(e), Error::Invalid(m) if m == "invalid value"));
    // not-null violation -> Invalid
    let e = bad(p, "INSERT INTO domains(name) VALUES ('nn.example')").await;
    assert_eq!(code_of(&e), "23502");
    assert!(matches!(map_db_error(e), Error::Invalid(m) if m == "invalid value"));
    // trigger conflict RS001 -> Conflict
    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('taken',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    let e = bad(
        p,
        &format!("INSERT INTO lists(local_part,domain_id) VALUES ('taken',{})", s.domain),
    )
    .await;
    assert_eq!(code_of(&e), "RS001");
    assert!(matches!(map_db_error(e), Error::Conflict(m) if m == "database conflict"));
    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn pg_map_db_error_messages_do_not_leak_server_text() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    seed(p).await;
    let secret = "S3CR3T-LEAK-MARKER";
    for sql in [
        format!("INSERT INTO domains(name,created_at) VALUES ('{secret}-UPPER',0)"),
        format!(
            "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('{secret}',999999,'active',0)"
        ),
        format!("SELECT {secret}_no_such_column FROM domains"),
        format!("INSERT INTO no_such_table_{secret} VALUES (1)"),
    ] {
        let mapped = map_db_error(bad(p, &sql).await);
        let text = mapped.to_string();
        assert!(!text.contains(secret), "leaked into Display: {text}");
        for banned in ["violates", "constraint", "relation", "column", "DETAIL", "Key ("] {
            assert!(!text.contains(banned), "server text {banned:?} in {text:?}");
        }
        assert!(text.len() < 80, "{text}");
    }
    store.close().await;
    db.cleanup().await;
}

/// TRUNCATE of an owner table clears that kind's registry rows (mutations
/// `accounts_addr_truncate` / `aliases_addr_truncate` / `lists_addr_truncate` dropped).
#[tokio::test]
async fn truncating_an_owner_table_clears_that_kinds_registry_rows() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    let owners = |kind: &'static str| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM address_owners WHERE owner_kind = $1")
            .bind(kind)
            .fetch_one(p)
            .await
            .unwrap()
    };
    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('li',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .unwrap();
    assert_eq!((owners("account").await, owners("alias").await, owners("list").await), (2, 1, 1));
    sqlx::query("TRUNCATE aliases").execute(p).await.unwrap();
    assert_eq!((owners("account").await, owners("alias").await, owners("list").await), (2, 0, 1));
    sqlx::query("TRUNCATE lists CASCADE").execute(p).await.unwrap();
    assert_eq!((owners("account").await, owners("alias").await, owners("list").await), (2, 0, 0));
    sqlx::query("TRUNCATE accounts CASCADE").execute(p).await.unwrap();
    assert_eq!((owners("account").await, owners("alias").await, owners("list").await), (0, 0, 0));
    store.close().await;
    db.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Drift: the table's own constraints when the registry triggers do not run.
//
// The address registry triggers are the first line of defence, but they are not always on:
// `pg_restore --disable-triggers`, `session_replication_role = replica`, logical-replication
// apply and an operator's `ALTER TABLE .. DISABLE TRIGGER` all bypass them. Then the unique keys
// and foreign keys of the owner tables are the ONLY barrier, so each is pinned here by name
// (review PG6 findings 1 and 2). Each helper reproduces the bypass with DISABLE TRIGGER.

/// SQLSTATE and constraint name of a rejected statement.
fn code_and_constraint(e: &sqlx::Error) -> (Option<String>, Option<String>) {
    let d = e.as_database_error().expect("a database error");
    (d.code().map(|c| c.to_string()), d.constraint().map(str::to_owned))
}

async fn without_trigger(pool: &PgPool, table: &str, trigger: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE {table} DISABLE TRIGGER {trigger}")))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_table_unique_keys_hold_when_the_registry_triggers_are_off() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    sqlx::query("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('taken',$1,$2)")
        .bind(s.domain)
        .bind(s.account_a)
        .execute(p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lists(local_part,domain_id) VALUES ('taken-list',$1)")
        .bind(s.domain)
        .execute(p)
        .await
        .unwrap();
    without_trigger(p, "accounts", "accounts_addr_unique_ins").await;
    without_trigger(p, "aliases", "aliases_addr_unique_ins").await;
    without_trigger(p, "lists", "lists_addr_unique_ins").await;

    let dup_account = bad(
        p,
        &format!("INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',{},'active',0)", s.domain),
    )
    .await;
    assert_eq!(
        code_and_constraint(&dup_account),
        (Some("23505".into()), Some("accounts_local_part_domain_id_key".into()))
    );
    let dup_alias = bad(
        p,
        &format!("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('taken',{},{})", s.domain, s.account_a),
    )
    .await;
    assert_eq!(
        code_and_constraint(&dup_alias),
        (Some("23505".into()), Some("aliases_local_part_domain_id_key".into()))
    );
    let dup_list = bad(
        p,
        &format!("INSERT INTO lists(local_part,domain_id) VALUES ('taken-list',{})", s.domain),
    )
    .await;
    assert_eq!(
        code_and_constraint(&dup_list),
        (Some("23505".into()), Some("lists_local_part_domain_id_key".into()))
    );
    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn alias_and_list_domain_foreign_keys_hold_when_the_registry_triggers_are_off() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    without_trigger(p, "aliases", "aliases_addr_unique_ins").await;
    without_trigger(p, "lists", "lists_addr_unique_ins").await;
    let no_domain = s.domain + 1000;
    let alias = bad(
        p,
        &format!("INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('x',{no_domain},{})", s.account_a),
    )
    .await;
    assert_eq!(
        code_and_constraint(&alias),
        (Some("23503".into()), Some("aliases_domain_id_fkey".into()))
    );
    let list = bad(p, &format!("INSERT INTO lists(local_part,domain_id) VALUES ('x',{no_domain})")).await;
    assert_eq!(
        code_and_constraint(&list),
        (Some("23503".into()), Some("lists_domain_id_fkey".into()))
    );
    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn registry_table_constraints_hold_when_its_guard_trigger_is_off() {
    let Some((db, store)) = database().await else {
        return;
    };
    let p = store.pool();
    let s = seed(p).await;
    without_trigger(p, "address_owners", "address_owners_guard").await;
    let kind = bad(
        p,
        &format!("INSERT INTO address_owners(domain_id,local_part,owner_kind,owner_id) VALUES ({},'k','bogus',1)", s.domain),
    )
    .await;
    assert_eq!(
        code_and_constraint(&kind),
        (Some("23514".into()), Some("address_owners_owner_kind_check".into()))
    );
    // account 'a' already owns (account, id): a second registry row for the same owner.
    let owner = bad(
        p,
        &format!(
            "INSERT INTO address_owners(domain_id,local_part,owner_kind,owner_id) VALUES ({},'again','account',{})",
            s.domain, s.account_a
        ),
    )
    .await;
    assert_eq!(
        code_and_constraint(&owner),
        (Some("23505".into()), Some("address_owners_owner_kind_owner_id_key".into()))
    );
    store.close().await;
    db.cleanup().await;
}
