mod common;

use common::mem;
use rusty_core::Error;
use rusty_store::{map_db_error, SqliteStore};
use sqlx::SqlitePool;

async fn ok(p: &SqlitePool, sql: &str) {
    if let Err(e) = sqlx::query(sql).execute(p).await {
        panic!("expected success for `{sql}`, got {e}");
    }
}

async fn bad(p: &SqlitePool, sql: &str) {
    assert!(sqlx::query(sql).execute(p).await.is_err(), "expected failure for `{sql}`");
}

async fn count(p: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(p).await.unwrap()
}

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// domain 1, accounts 1 and 2, mailboxes 1 and 2, folder (1,1) 'INBOX' root, blobs A and B.
async fn seed() -> SqliteStore {
    let s = mem().await;
    let p = s.pool();
    ok(p, "INSERT INTO domains(id,name,created_at) VALUES (1,'example.com',0)").await;
    ok(p, "INSERT INTO accounts(id,local_part,domain_id,status,created_at) VALUES (1,'a',1,'active',0)").await;
    ok(p, "INSERT INTO accounts(id,local_part,domain_id,status,created_at) VALUES (2,'b',1,'active',0)").await;
    ok(p, "INSERT INTO mailboxes(id,account_id,uid_validity) VALUES (1,1,1)").await;
    ok(p, "INSERT INTO mailboxes(id,account_id,uid_validity) VALUES (2,2,1)").await;
    ok(p, "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) VALUES (1,1,NULL,'INBOX','inbox',1)").await;
    ok(p, "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) VALUES (2,1,NULL,'INBOX','inbox',1)").await;
    ok(p, &format!("INSERT INTO blobs(hash,size) VALUES ('{HASH_A}',10)")).await;
    ok(p, &format!("INSERT INTO blobs(hash,size) VALUES ('{HASH_B}',10)")).await;
    s
}

fn msg(mailbox: i64, id: i64, folder: i64, uid: i64, hash: &str, size: i64, flags: i64) -> String {
    format!(
        "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id,flags) \
         VALUES ({mailbox},{id},{folder},{uid},'{hash}',{size},0,1,{flags})"
    )
}

fn folder(mailbox: i64, id: i64, parent: &str, name: &str, role: &str) -> String {
    format!(
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
         VALUES ({mailbox},{id},{parent},'{name}','{role}',1)"
    )
}

#[tokio::test]
async fn rejects_uppercase_domain() {
    let s = mem().await;
    bad(s.pool(), "INSERT INTO domains(name,created_at) VALUES ('Example.COM',0)").await;
    ok(s.pool(), "INSERT INTO domains(name,created_at) VALUES ('example.com',0)").await;
}

#[tokio::test]
async fn rejects_account_with_missing_domain() {
    let s = mem().await;
    let p = s.pool();
    bad(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',99,'active',0)").await;
    ok(p, "INSERT INTO domains(id,name,created_at) VALUES (99,'x.org',0)").await;
    ok(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',99,'active',0)").await;
}

#[tokio::test]
async fn rejects_delete_domain_with_accounts() {
    let s = seed().await;
    let p = s.pool();
    bad(p, "DELETE FROM domains WHERE id=1").await;
    ok(p, "INSERT INTO domains(id,name,created_at) VALUES (2,'empty.org',0)").await;
    ok(p, "DELETE FROM domains WHERE id=2").await;
}

#[tokio::test]
async fn rejects_list_member_with_both_account_and_external() {
    let s = seed().await;
    let p = s.pool();
    ok(p, "INSERT INTO lists(id,local_part,domain_id) VALUES (1,'team',1)").await;
    bad(p, "INSERT INTO list_members(list_id,account_id,external) VALUES (1,1,'x@y.org')").await;
    ok(p, "INSERT INTO list_members(list_id,account_id,external) VALUES (1,1,NULL)").await;
}

#[tokio::test]
async fn rejects_list_member_with_neither() {
    let s = seed().await;
    let p = s.pool();
    ok(p, "INSERT INTO lists(id,local_part,domain_id) VALUES (1,'team',1)").await;
    bad(p, "INSERT INTO list_members(list_id,account_id,external) VALUES (1,NULL,NULL)").await;
    ok(p, "INSERT INTO list_members(list_id,account_id,external) VALUES (1,NULL,'x@y.org')").await;
}

#[tokio::test]
async fn rejects_duplicate_folder_name_under_same_parent() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "1", "Work", "custom")).await;
    bad(p, &folder(1, 3, "1", "Work", "custom")).await;
    ok(p, &folder(1, 3, "1", "Play", "custom")).await;
}

#[tokio::test]
async fn rejects_duplicate_folder_name_at_root() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "NULL", "Work", "custom")).await;
    bad(p, &folder(1, 3, "NULL", "Work", "custom")).await;
    ok(p, &folder(1, 3, "NULL", "Play", "custom")).await;
}

#[tokio::test]
async fn rejects_two_trash_folders() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "NULL", "Trash", "trash")).await;
    bad(p, &folder(1, 3, "NULL", "Trash2", "trash")).await;
    // another mailbox may have its own trash
    ok(p, &folder(2, 2, "NULL", "Trash", "trash")).await;
}

#[tokio::test]
async fn allows_many_custom_folders() {
    let s = seed().await;
    let p = s.pool();
    for i in 2..12 {
        ok(p, &folder(1, i, "NULL", &format!("f{i}"), "custom")).await;
    }
    assert_eq!(count(p, "SELECT count(*) FROM folders WHERE role='custom' AND mailbox_id=1").await, 10);
}

#[tokio::test]
async fn rejects_folder_parent_equal_self() {
    let s = seed().await;
    let p = s.pool();
    bad(p, &folder(1, 2, "2", "Loop", "custom")).await;
    ok(p, &folder(1, 2, "1", "Loop", "custom")).await;
}

#[tokio::test]
async fn rejects_folder_cycle() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "1", "A", "custom")).await;
    ok(p, &folder(1, 3, "2", "B", "custom")).await;
    ok(p, &folder(1, 4, "3", "C", "custom")).await;
    // 2 -> 4 would create 2 -> 4 -> 3 -> 2
    bad(p, "UPDATE folders SET parent_id=4 WHERE mailbox_id=1 AND id=2").await;
    // valid reparent still works
    ok(p, "UPDATE folders SET parent_id=NULL WHERE mailbox_id=1 AND id=3").await;
    ok(p, "UPDATE folders SET parent_id=4 WHERE mailbox_id=1 AND id=2").await;
}

#[tokio::test]
async fn rejects_folder_tree_deeper_than_64() {
    let s = seed().await;
    let p = s.pool();
    // chain 1 <- 2 <- ... <- 70 : 70 levels, built via inserts (the depth trigger only guards UPDATE OF parent_id)
    for i in 2..=70 {
        ok(p, &folder(1, i, &(i - 1).to_string(), &format!("d{i}"), "custom")).await;
    }
    ok(p, &folder(1, 100, "NULL", "free", "custom")).await;
    // attaching a new root under the deep tip makes depth > 64
    bad(p, "UPDATE folders SET parent_id=70 WHERE mailbox_id=1 AND id=100").await;
    ok(p, "UPDATE folders SET parent_id=2 WHERE mailbox_id=1 AND id=100").await;
}

#[tokio::test]
async fn rejects_message_referencing_other_mailbox_folder() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(2, 2, "NULL", "Other", "custom")).await; // folder (2,2) exists only in mailbox 2
    bad(p, &msg(1, 1, 2, 1, HASH_A, 5, 0)).await;
    ok(p, &msg(1, 1, 1, 1, HASH_A, 5, 0)).await;
    ok(p, &msg(2, 1, 2, 1, HASH_A, 5, 0)).await;
}

#[tokio::test]
async fn rejects_message_size_zero() {
    let s = seed().await;
    bad(s.pool(), &msg(1, 1, 1, 1, HASH_A, 0, 0)).await;
    ok(s.pool(), &msg(1, 1, 1, 1, HASH_A, 1, 0)).await;
}

#[tokio::test]
async fn rejects_flags_over_31() {
    let s = seed().await;
    bad(s.pool(), &msg(1, 1, 1, 1, HASH_A, 5, 32)).await;
    ok(s.pool(), &msg(1, 1, 1, 1, HASH_A, 5, 31)).await;
}

#[tokio::test]
async fn rejects_invalid_blob_hash() {
    let s = mem().await;
    let p = s.pool();
    bad(p, "INSERT INTO blobs(hash,size) VALUES ('abc',1)").await;
    let upper = HASH_A.to_uppercase();
    bad(p, &format!("INSERT INTO blobs(hash,size) VALUES ('{upper}',1)")).await;
    let nonhex = format!("{}g", &HASH_A[..63]);
    bad(p, &format!("INSERT INTO blobs(hash,size) VALUES ('{nonhex}',1)")).await;
    ok(p, &format!("INSERT INTO blobs(hash,size) VALUES ('{HASH_A}',1)")).await;
}

#[tokio::test]
async fn rejects_negative_refcount() {
    let s = mem().await;
    let p = s.pool();
    bad(p, &format!("INSERT INTO blobs(hash,size,refcount) VALUES ('{HASH_A}',1,-1)")).await;
    ok(p, &format!("INSERT INTO blobs(hash,size,refcount) VALUES ('{HASH_A}',1,0)")).await;
    bad(p, &format!("UPDATE blobs SET refcount=-1 WHERE hash='{HASH_A}'")).await;
}

#[tokio::test]
async fn rejects_delete_folder_with_messages() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "NULL", "Work", "custom")).await;
    ok(p, &msg(1, 1, 2, 1, HASH_A, 5, 0)).await;
    bad(p, "DELETE FROM folders WHERE mailbox_id=1 AND id=2").await;
    ok(p, "DELETE FROM messages WHERE mailbox_id=1 AND id=1").await;
    ok(p, "DELETE FROM folders WHERE mailbox_id=1 AND id=2").await;
}

#[tokio::test]
async fn rejects_address_shared_by_account_and_alias() {
    let s = seed().await;
    let p = s.pool();
    bad(p, "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('a',1,2)").await;
    ok(p, "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('a2',1,2)").await;
    // reverse direction
    bad(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a2',1,'active',0)").await;
}

#[tokio::test]
async fn rejects_address_shared_by_alias_and_list() {
    let s = seed().await;
    let p = s.pool();
    ok(p, "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',1,1)").await;
    bad(p, "INSERT INTO lists(local_part,domain_id) VALUES ('al',1)").await;
    ok(p, "INSERT INTO lists(local_part,domain_id) VALUES ('li',1)").await;
    bad(p, "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('li',1,1)").await;
}

#[tokio::test]
async fn rejects_address_shared_by_account_and_list() {
    let s = seed().await;
    let p = s.pool();
    bad(p, "INSERT INTO lists(local_part,domain_id) VALUES ('a',1)").await;
    ok(p, "INSERT INTO lists(local_part,domain_id) VALUES ('li',1)").await;
    bad(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('li',1,'active',0)").await;
}

#[tokio::test]
async fn rejects_address_collision_via_update() {
    let s = seed().await;
    let p = s.pool();
    ok(p, "INSERT INTO lists(id,local_part,domain_id) VALUES (1,'li',1)").await;
    ok(p, "INSERT INTO aliases(id,local_part,domain_id,account_id) VALUES (1,'al',1,1)").await;
    bad(p, "UPDATE lists SET local_part='a' WHERE id=1").await;
    bad(p, "UPDATE aliases SET local_part='li' WHERE id=1").await;
    bad(p, "UPDATE accounts SET local_part='al' WHERE id=2").await;
    ok(p, "UPDATE lists SET local_part='li2' WHERE id=1").await;
    ok(p, "UPDATE accounts SET local_part='b2' WHERE id=2").await;
}

#[tokio::test]
async fn same_local_part_in_different_domains_is_allowed() {
    let s = seed().await;
    let p = s.pool();
    ok(p, "INSERT INTO domains(id,name,created_at) VALUES (2,'other.org',0)").await;
    ok(p, "INSERT INTO lists(local_part,domain_id) VALUES ('a',2)").await;
}

#[tokio::test]
async fn rejects_invalid_json_config() {
    let s = mem().await;
    let p = s.pool();
    bad(p, "INSERT INTO domains(name,config,created_at) VALUES ('x.org','{not json',0)").await;
    ok(p, "INSERT INTO domains(name,config,created_at) VALUES ('x.org','{\"a\":1}',0)").await;
    bad(p, "INSERT INTO plans(name,config) VALUES ('p','nope')").await;
    ok(p, "INSERT INTO plans(name,config) VALUES ('p','{}')").await;
}

#[tokio::test]
async fn rejects_change_log_bad_op() {
    let s = seed().await;
    let p = s.pool();
    bad(p, "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'message',1,'bogus')").await;
    ok(p, "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'message',1,'create')").await;
    bad(p, "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,2,'weird',1,'create')").await;
}

#[tokio::test]
async fn refcount_follows_message_insert_update_delete() {
    let s = seed().await;
    let p = s.pool();
    async fn rc(p: &SqlitePool, h: &str) -> i64 {
        count(p, &format!("SELECT refcount FROM blobs WHERE hash='{h}'")).await
    }
    let (a, b) = (HASH_A, HASH_B);
    assert_eq!(rc(p, a).await, 0);
    ok(p, &msg(1, 1, 1, 1, HASH_A, 5, 0)).await;
    assert_eq!((rc(p, a).await, rc(p, b).await), (1, 0));
    ok(p, &msg(1, 2, 1, 2, HASH_A, 5, 0)).await;
    assert_eq!(rc(p, a).await, 2);
    ok(p, &format!("UPDATE messages SET blob_hash='{HASH_B}' WHERE mailbox_id=1 AND id=1")).await;
    assert_eq!((rc(p, a).await, rc(p, b).await), (1, 1));
    ok(p, "DELETE FROM messages WHERE mailbox_id=1 AND id=1").await;
    assert_eq!((rc(p, a).await, rc(p, b).await), (1, 0));
    ok(p, "DELETE FROM messages WHERE mailbox_id=1 AND id=2").await;
    assert_eq!((rc(p, a).await, rc(p, b).await), (0, 0));
}

#[tokio::test]
async fn refcount_follows_cascade_delete_of_mailbox() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &msg(1, 1, 1, 1, HASH_A, 5, 0)).await;
    ok(p, "DELETE FROM accounts WHERE id=1").await;
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash='{HASH_A}'")).await, 0);
}

#[tokio::test]
async fn open_enables_foreign_keys_on_every_pooled_connection() {
    // A failing PRAGMA cannot be simulated through the public API (open forces the
    // option on every connection), so we assert the positive property instead: all
    // 4 connections of a file-backed pool report foreign_keys = 1, and the FK is enforced.
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("t.db").display());
    let s = SqliteStore::open(&url).await.unwrap();
    let mut conns = Vec::new();
    for _ in 0..4 {
        conns.push(s.pool().acquire().await.unwrap());
    }
    for c in conns.iter_mut() {
        let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys").fetch_one(&mut **c).await.unwrap();
        assert_eq!(on, 1);
    }
    drop(conns);
    bad(s.pool(), "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',99,'active',0)").await;
}

#[tokio::test]
async fn open_reports_storage_error_for_unusable_path() {
    let r = SqliteStore::open("sqlite:///nonexistent-dir-xyz/sub/t.db").await;
    assert!(matches!(r, Err(Error::Storage { .. })));
}

#[tokio::test]
async fn map_db_error_classifies_constraint_violations() {
    let s = seed().await;
    let p = s.pool();
    async fn err(p: &SqlitePool, sql: &str) -> sqlx::Error {
        sqlx::query(sql).execute(p).await.unwrap_err()
    }

    // UNIQUE
    let e = err(p, "INSERT INTO domains(name,created_at) VALUES ('example.com',0)").await;
    assert!(matches!(map_db_error(e), Error::Conflict(_)));
    // PRIMARY KEY
    let e = err(p, "INSERT INTO domains(id,name,created_at) VALUES (1,'z.org',0)").await;
    assert!(matches!(map_db_error(e), Error::Conflict(_)));
    // FOREIGN KEY
    let e = err(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('q',99,'active',0)").await;
    assert!(matches!(map_db_error(e), Error::Conflict(_)));
    // CHECK
    let e = err(p, "INSERT INTO domains(name,created_at) VALUES ('UP.org',0)").await;
    assert!(matches!(map_db_error(e), Error::Invalid(_)));
    // NOT NULL
    let e = err(p, "INSERT INTO domains(name) VALUES ('nn.org')").await;
    assert!(matches!(map_db_error(e), Error::Invalid(_)));
    // trigger RAISE
    let e = err(p, "INSERT INTO lists(local_part,domain_id) VALUES ('a',1)").await;
    assert!(matches!(map_db_error(e), Error::Conflict(_))); // address-collision triggers use "conflict:"
    // trigger RAISE without the prefix -> Invalid, and the text does not contain "constraint failed"
    ok(p, &folder(1, 2, "1", "A", "custom")).await;
    let e = err(p, "UPDATE folders SET parent_id=2 WHERE mailbox_id=1 AND id=1").await;
    match map_db_error(e) {
        Error::Invalid(m) => assert!(!m.contains("constraint failed") && m.contains("cycle"), "{m}"),
        other => panic!("{other:?}"),
    }
    // non-constraint errors are Storage
    let e = err(p, "SELECT * FROM no_such_table").await;
    assert!(matches!(map_db_error(e), Error::Storage { .. }));
    // positive control: the valid variant succeeds
    ok(p, "INSERT INTO lists(local_part,domain_id) VALUES ('fine',1)").await;
}

#[tokio::test]
async fn map_db_error_messages_do_not_leak_sqlite_text() {
    let s = seed().await;
    let p = s.pool();
    for sql in [
        "INSERT INTO domains(name,created_at) VALUES ('example.com',0)",
        "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('q',99,'active',0)",
        "INSERT INTO domains(name,created_at) VALUES ('UP.org',0)",
        "INSERT INTO domains(name) VALUES ('nn.org')",
        "INSERT INTO lists(local_part,domain_id) VALUES ('a',1)",
    ] {
        let m = map_db_error(sqlx::query(sql).execute(p).await.unwrap_err()).to_string();
        assert!(!m.contains("constraint failed") && !m.contains("domains.") && !m.contains("UNIQUE"), "{m}");
    }
}

#[tokio::test]
async fn delete_account_with_nested_folders_and_messages_succeeds() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "1", "L2", "custom")).await;
    ok(p, &folder(1, 3, "2", "L3", "custom")).await;
    ok(p, &msg(1, 1, 1, 1, HASH_A, 5, 0)).await;
    ok(p, &msg(1, 2, 2, 1, HASH_A, 5, 0)).await;
    ok(p, &msg(1, 3, 3, 1, HASH_B, 5, 0)).await;
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash='{HASH_A}'")).await, 2);
    ok(p, "DELETE FROM accounts WHERE id=1").await;
    assert_eq!(count(p, "SELECT count(*) FROM mailboxes WHERE id=1").await, 0);
    assert_eq!(count(p, "SELECT count(*) FROM folders WHERE mailbox_id=1").await, 0);
    assert_eq!(count(p, "SELECT count(*) FROM messages WHERE mailbox_id=1").await, 0);
    assert_eq!(count(p, "SELECT sum(refcount) FROM blobs").await, 0);
    // other account untouched
    assert_eq!(count(p, "SELECT count(*) FROM folders WHERE mailbox_id=2").await, 1);
}

#[tokio::test]
async fn rejects_delete_folder_with_children() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &folder(1, 2, "1", "Child", "custom")).await;
    bad(p, "DELETE FROM folders WHERE mailbox_id=1 AND id=1").await;
    ok(p, "DELETE FROM folders WHERE mailbox_id=1 AND id=2").await;
    ok(p, "DELETE FROM folders WHERE mailbox_id=1 AND id=1").await;
}

#[tokio::test]
async fn rejects_address_collision_via_domain_id_update() {
    let s = seed().await;
    let p = s.pool();
    ok(p, "INSERT INTO domains(id,name,created_at) VALUES (2,'other.org',0)").await;
    ok(p, "INSERT INTO lists(id,local_part,domain_id) VALUES (1,'a',2)").await;
    // moving list a to domain 1 would collide with account a@example.com
    bad(p, "UPDATE lists SET domain_id=1 WHERE id=1").await;
    ok(p, "UPDATE lists SET local_part='a2', domain_id=1 WHERE id=1").await;
}

#[tokio::test]
async fn rejects_keyword_not_lowercase() {
    let s = seed().await;
    let p = s.pool();
    ok(p, &msg(1, 1, 1, 1, HASH_A, 5, 0)).await;
    bad(p, "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'Urgent')").await;
    ok(p, "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'urgent')").await;
    bad(p, "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'')").await;
}

#[tokio::test]
async fn rejects_folder_name_with_slash() {
    let s = seed().await;
    let p = s.pool();
    bad(p, &folder(1, 2, "NULL", "a/b", "custom")).await;
    ok(p, &folder(1, 2, "NULL", "ab", "custom")).await;
}

#[tokio::test]
async fn rejects_invalid_account_status() {
    let s = seed().await;
    let p = s.pool();
    bad(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('z',1,'bogus',0)").await;
    ok(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('z',1,'locked',0)").await;
}

#[tokio::test]
async fn rejects_invalid_folder_role() {
    let s = seed().await;
    let p = s.pool();
    bad(p, &folder(1, 2, "NULL", "X", "bogus")).await;
    ok(p, &folder(1, 2, "NULL", "X", "archive")).await;
}

#[tokio::test]
async fn rejects_message_uid_zero() {
    let s = seed().await;
    let p = s.pool();
    bad(p, &msg(1, 1, 1, 0, HASH_A, 5, 0)).await;
    ok(p, &msg(1, 1, 1, 1, HASH_A, 5, 0)).await;
}
