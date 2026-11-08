//! backend-neutral: Postgres-only suite (needs a server; compares against the SQLite behaviour elsewhere)
#![cfg(feature = "postgres")]

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
    sqlx::query(sql).execute(pool).await.unwrap_err()
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

// ---------------------------------------------------------------------------------------------
// Completeness of the negative-test port (plan PG6, review finding 4).
//
// Every test of the SQLite suites `schema_constraints.rs` and `schema_capabilities.rs` has an
// entry here saying which Postgres test or table case covers it, with the exact SQLSTATE and
// constraint name asserted in `pg_schema_cases.rs` (`run_case` compares both), or why it has no
// Postgres twin. `sqlite_negative_tests_all_have_a_postgres_twin` fails when a SQLite test is
// added without an entry, when an entry names a SQLite test that no longer exists, and when a
// named Postgres case or test does not exist.

enum Twin {
    /// Names of cases in `pg_schema_cases.rs` `cases()`.
    Cases(&'static [&'static str]),
    /// Names of `#[tokio::test]` functions in the `pg_*.rs` suites.
    Tests(&'static [&'static str]),
    /// No Postgres twin, with the reason.
    NotApplicable(&'static str),
}
use Twin::{Cases, NotApplicable, Tests};

const LIFECYCLE: &str = "refcount_and_zero_since_follow_message_lifecycle";
const CASCADES: &str = "cascades_release_everything_and_other_tenants_are_untouched";
const FROZEN_MIGRATION: &str = "SQLite migration history is frozen and replayed only on SQLite; \
Postgres has the single 0001 schema, which creates the final state directly";

const PORTED: &[(&str, Twin)] = &[
    ("rejects_uppercase_domain", Cases(&["domain uppercase"])),
    ("rejects_account_with_missing_domain", Cases(&["account missing domain"])),
    ("rejects_delete_domain_with_accounts", Cases(&["domain delete with accounts"])),
    ("rejects_list_member_with_both_account_and_external", Cases(&["member with both account and external"])),
    ("rejects_list_member_with_neither", Cases(&["member with neither"])),
    ("rejects_duplicate_folder_name_under_same_parent", Cases(&["folder duplicate sibling name"])),
    ("rejects_duplicate_folder_name_at_root", Cases(&["folder duplicate root name"])),
    ("rejects_two_trash_folders", Cases(&["two trash folders", "trash in two mailboxes"])),
    ("allows_many_custom_folders", Cases(&["many custom folders"])),
    ("rejects_folder_parent_equal_self", Cases(&["folder parent equal self (insert)", "folder reparent to self"])),
    ("rejects_folder_cycle", Cases(&["folder cycle of three", "long cycle (30 levels) is found"])),
    ("rejects_folder_tree_deeper_than_64", Cases(&["depth 64 is allowed", "depth 65 is rejected", "deeper than 64 (70 levels) rejected"])),
    ("rejects_message_referencing_other_mailbox_folder", Cases(&["message folder of another mailbox"])),
    ("rejects_message_size_zero", Cases(&["message size 0"])),
    ("rejects_flags_over_31", Cases(&["message flags 32", "message flags -1"])),
    ("rejects_invalid_blob_hash", Cases(&["blob short hash", "blob 33-byte hash"])),
    ("rejects_negative_refcount", Cases(&["blob insert negative refcount", "blob update negative refcount"])),
    ("rejects_delete_folder_with_messages", Cases(&["folder delete with messages"])),
    ("rejects_address_shared_by_account_and_alias", Cases(&["account vs alias", "alias then account of same address"])),
    ("rejects_address_shared_by_alias_and_list", Cases(&["alias vs list"])),
    ("rejects_address_shared_by_account_and_list", Cases(&["account vs list", "list then account of same address"])),
    ("rejects_address_collision_via_update", Cases(&["account update collides with account", "account update collides with list", "account update collides with alias", "alias update collides", "list update collides"])),
    ("same_local_part_in_different_domains_is_allowed", Cases(&["account same local part, other domain"])),
    ("rejects_invalid_json_config", Cases(&["domain bad json", "plan bad json", "account bad json config"])),
    ("rejects_change_log_bad_op", Cases(&["change_log bad op"])),
    ("refcount_follows_message_insert_update_delete", Tests(&[LIFECYCLE])),
    ("refcount_follows_cascade_delete_of_mailbox", Tests(&[CASCADES])),
    ("open_enables_foreign_keys_on_every_pooled_connection", NotApplicable("Postgres always enforces foreign keys (no per-connection pragma); enforcement itself is asserted by the FK cases, e.g. 'message missing blob'")),
    ("open_reports_storage_error_for_unusable_path", Tests(&["wrong_password_is_sanitized"])),
    ("map_db_error_classifies_constraint_violations", Tests(&["pg_map_db_error_classifies_constraint_violations_by_sqlstate"])),
    ("map_db_error_messages_do_not_leak_sqlite_text", Tests(&["pg_map_db_error_messages_do_not_leak_server_text"])),
    ("delete_account_with_nested_folders_and_messages_succeeds", Tests(&[CASCADES])),
    ("rejects_delete_folder_with_children", Cases(&["folder delete with children"])),
    ("rejects_address_collision_via_domain_id_update", Cases(&["account domain_id update collides", "alias domain_id update collides", "list domain_id update collides"])),
    ("rejects_keyword_not_lowercase", Cases(&["keyword uppercase"])),
    ("rejects_folder_name_with_slash", Cases(&["folder name with slash"])),
    ("rejects_invalid_account_status", Cases(&["account bad status"])),
    ("rejects_invalid_folder_role", Cases(&["folder bad role"])),
    ("rejects_message_uid_zero", Cases(&["message uid 0"])),
    ("zero_since_is_set_when_blob_inserted_unreferenced", Tests(&[LIFECYCLE])),
    ("zero_since_follows_message_insert_update_delete", Tests(&[LIFECYCLE])),
    ("zero_since_set_again_after_cascade_delete_of_account", Tests(&[CASCADES])),
    ("rejects_negative_zero_since", Cases(&["blob negative zero_since"])),
    ("messages_have_partial_index_on_deleted_at", Tests(&["pg_schema_matches_sqlite_structure"])),
    ("migration_0002_backfills_zero_since", NotApplicable(FROZEN_MIGRATION)),
    ("migration_0008_backfills_list_member_sequence", NotApplicable(FROZEN_MIGRATION)),
    ("deliveries_accepts_valid_row_and_rejects_duplicate_key", Cases(&["delivery ok", "delivery duplicate key", "delivery second id"])),
    ("deliveries_rejects_bad_id_length_and_hash_length", Cases(&["delivery short id", "delivery 33-byte id", "delivery short hash"])),
    ("deliveries_rejects_missing_or_foreign_message", Cases(&["delivery missing message", "delivery foreign mailbox", "delivery missing mailbox"])),
    ("deliveries_cascade_with_message_and_mailbox", Tests(&[CASCADES])),
    ("messages_blob_hash_is_indexed_for_the_gc_foreign_key_probe", Tests(&["gc_delete_foreign_key_probe_uses_the_blob_hash_index"])),
    ("capabilities_table_accepts_valid_row", Cases(&["capability ok"])),
    ("capability_with_mismatched_domain_is_rejected", Cases(&["capability account in other domain"])),
    ("capability_row_constraints", Cases(&["capability 31-byte hash", "capability 33-byte hash", "capability role admin", "capability role IMAP", "capability expiry == issue", "capability expiry < issue", "capability lifetime 86401", "capability revoked before issue", "capability duplicate hash"])),
    ("deleting_account_cascades_to_capabilities", Tests(&[CASCADES])),
    ("accounts_id_domain_unique_index_exists", Tests(&["pg_schema_matches_sqlite_structure"])),
];

/// Names of the test functions in a source file (attribute line followed by `fn name`).
fn test_fns(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = vec![];
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("#[tokio::test") || t.starts_with("#[test") {
            for next in lines.iter().skip(i + 1).take(6) {
                let n = next.trim_start();
                let n = n.strip_prefix("pub ").unwrap_or(n);
                let n = n.strip_prefix("async ").unwrap_or(n);
                if let Some(rest) = n.strip_prefix("fn ") {
                    out.push(rest.split('(').next().unwrap_or("").to_owned());
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn sqlite_negative_tests_all_have_a_postgres_twin() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_else(|_| panic!("{f}"));
    let mut sqlite: Vec<String> = test_fns(&read("schema_constraints.rs"));
    sqlite.extend(test_fns(&read("schema_capabilities.rs")));
    sqlite.sort();
    let mut ported: Vec<String> = PORTED.iter().map(|(n, _)| (*n).to_owned()).collect();
    ported.sort();
    let dup: Vec<_> = ported.windows(2).filter(|w| w[0] == w[1]).collect();
    assert!(dup.is_empty(), "duplicate PORTED entries: {dup:?}");
    let missing: Vec<_> = sqlite.iter().filter(|n| !ported.contains(n)).collect();
    let extra: Vec<_> = ported.iter().filter(|n| !sqlite.contains(n)).collect();
    assert!(missing.is_empty(), "SQLite tests without a Postgres twin entry: {missing:?}");
    assert!(extra.is_empty(), "PORTED entries for tests that no longer exist: {extra:?}");

    let cases_src = read("pg_schema_cases.rs");
    let mut pg_tests: Vec<String> = vec![];
    for f in [
        "pg_schema_cases.rs",
        "pg_schema_constraints.rs",
        "pg_schema_parity.rs",
        "pg_plans.rs",
        "pg_smoke.rs",
        "pg_schema_concurrency.rs",
    ] {
        pg_tests.extend(test_fns(&read(f)));
    }
    for (name, twin) in PORTED {
        match twin {
            Cases(cases) => {
                assert!(!cases.is_empty(), "{name}: empty case list");
                for c in *cases {
                    let quoted = format!("\"{c}\"");
                    assert!(
                        cases_src.contains(&format!("case({quoted}"))
                            || cases_src.contains(&format!("with({quoted}")),
                        "{name}: no case named {c:?} in pg_schema_cases.rs"
                    );
                }
            }
            Tests(tests) => {
                for t in *tests {
                    assert!(pg_tests.iter().any(|x| x == t), "{name}: no Postgres test named {t}");
                }
            }
            NotApplicable(reason) => assert!(reason.len() > 20, "{name}: give a real reason"),
        }
    }
}

// The production error mapping (`map_db_error`) classifies Postgres errors by SQLSTATE only and
// answers with fixed texts. SQLite twin: `map_db_error_classifies_constraint_violations`.
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
