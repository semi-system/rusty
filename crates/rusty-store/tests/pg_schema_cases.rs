//! Table-driven negative/positive schema cases for the Postgres schema. Ports every case of the
//! former SQLite suites and adds the cases needed to kill the mutants found in review. Each case runs in its own rolled-back transaction on one
//! seeded database; every rejection asserts the expected SQLSTATE and, where the engine reports
//! one, the constraint name, so a case cannot pass for the wrong reason.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::pg::{pg_admin_url, PgTestDb};
use sqlx::PgPool;

const CHECK: &str = "23514";
const UNIQUE: &str = "23505";
const FK: &str = "23503";
const RESTRICT: &str = "23001";
const NOTNULL: &str = "23502";
const BAD_JSON: &str = "22P02";
const ALWAYS: &str = "428C9";
const RS001: &str = "RS001";
const RS002: &str = "RS002";
const RS003: &str = "RS003";

const A: &str = "decode(repeat('aa',32),'hex')";
const B: &str = "decode(repeat('bb',32),'hex')";

#[derive(Clone, Copy)]
enum Exp {
    Ok,
    Err(&'static str, Option<&'static str>),
}
use Exp::{Err, Ok as Pass};

struct Case {
    name: String,
    setup: Vec<String>,
    sql: String,
    exp: Exp,
}

fn case(name: &str, sql: &str, exp: Exp) -> Case {
    Case {
        name: name.into(),
        setup: vec![],
        sql: sql.into(),
        exp,
    }
}

fn with(name: &str, setup: &[&str], sql: &str, exp: Exp) -> Case {
    Case {
        name: name.into(),
        setup: setup.iter().map(|s| (*s).into()).collect(),
        sql: sql.into(),
        exp,
    }
}

fn msg(mailbox: i64, id: i64, folder: i64, uid: i64, hash: &str, size: i64, flags: i64) -> String {
    format!(
        "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id,flags) \
         VALUES ({mailbox},{id},{folder},{uid},{hash},{size},0,1,{flags})"
    )
}

fn fol(mailbox: i64, id: i64, parent: &str, name: &str, role: &str) -> String {
    format!(
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
         VALUES ({mailbox},{id},{parent},'{name}','{role}',1)"
    )
}

fn job(cols: &str, vals: &str) -> String {
    format!("INSERT INTO jobs({cols},run_after,created_at,updated_at) VALUES ({vals},0,0,0)")
}

fn cap(token: &str, account: i64, domain: i64, role: &str, issued: i64, expires: i64) -> String {
    format!(
        "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
         VALUES ({token},{account},{domain},'{role}',{issued},{expires})"
    )
}

fn delivery(mailbox: i64, id: &str, message: i64, hash: &str) -> String {
    format!(
        "INSERT INTO deliveries(mailbox_id,delivery_id,message_id,blob_hash,created_at) \
         VALUES ({mailbox},{id},{message},{hash},0)"
    )
}

const D1: &str = "decode(repeat('01',32),'hex')";

/// domains 1 example.com / 2 other.org, plan 1, accounts 1 'a' and 2 'b' (domain 1), mailboxes
/// 1 and 2, folder (m,1) INBOX in each, blobs A and B (unreferenced). Identity sequences are
/// moved past the explicit ids so later generated ids cannot collide.
async fn seed(p: &PgPool) {
    let statements = [
        "INSERT INTO domains(id,name,created_at) OVERRIDING SYSTEM VALUE VALUES (1,'example.com',0),(2,'other.org',0)",
        "INSERT INTO plans(id,name) OVERRIDING SYSTEM VALUE VALUES (1,'basic')",
        "INSERT INTO accounts(id,local_part,domain_id,status,created_at) OVERRIDING SYSTEM VALUE \
         VALUES (1,'a',1,'active',0),(2,'b',1,'active',0)",
        "INSERT INTO mailboxes(id,account_id,uid_validity) OVERRIDING SYSTEM VALUE VALUES (1,1,1),(2,2,1)",
        "INSERT INTO folders(mailbox_id,id,parent_id,name,role,uid_validity) \
         VALUES (1,1,NULL,'INBOX','inbox',1),(2,1,NULL,'INBOX','inbox',1)",
        "INSERT INTO blobs(hash,size) VALUES (decode(repeat('aa',32),'hex'),10),(decode(repeat('bb',32),'hex'),10)",
    ];
    for s in statements {
        sqlx::query(s).execute(p).await.expect(s);
    }
    for t in ["domains", "plans", "accounts", "aliases", "lists", "mailboxes", "jobs"] {
        sqlx::query(&format!(
            "SELECT setval(pg_get_serial_sequence('{t}','id'), 1000)"
        ))
        .execute(p)
        .await
        .unwrap();
    }
}

/// 64 levels: folder 1 (root, seeded) then 2..=64, each child of the previous one, plus a free
/// root 100. Built by INSERT (the depth guard is an UPDATE OF parent_id trigger).
const CHAIN_64: &str = "INSERT INTO folders(mailbox_id,id,parent_id,name,uid_validity) \
    SELECT 1, g, g-1, 'd'||g, 1 FROM generate_series(2,64) g";
const CHAIN_30: &str = "INSERT INTO folders(mailbox_id,id,parent_id,name,uid_validity) \
    SELECT 1, g, g-1, 'd'||g, 1 FROM generate_series(2,30) g";
const CHAIN_70: &str = "INSERT INTO folders(mailbox_id,id,parent_id,name,uid_validity) \
    SELECT 1, g, g-1, 'd'||g, 1 FROM generate_series(2,70) g";
const CHAIN_70_MB2: &str = "INSERT INTO folders(mailbox_id,id,parent_id,name,uid_validity) \
    SELECT 2, g, g-1, 'd'||g, 1 FROM generate_series(2,70) g";
const FREE_100: &str = "INSERT INTO folders(mailbox_id,id,parent_id,name,uid_validity) VALUES (1,100,NULL,'free',1)";
const LIST_T: &str = "INSERT INTO lists(local_part,domain_id) VALUES ('t',1)";
const ALIAS_AL: &str = "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',1,1)";

#[allow(clippy::vec_init_then_push)]
fn cases() -> Vec<Case> {
    let mut v = vec![];

    // ---- domains / plans / identity
    v.push(case("domain uppercase", "INSERT INTO domains(name,created_at) VALUES ('UP.example',0)", Err(CHECK, Some("domains_name_valid"))));
    v.push(case("domain empty", "INSERT INTO domains(name,created_at) VALUES ('',0)", Err(CHECK, Some("domains_name_valid"))));
    v.push(case("domain duplicate", "INSERT INTO domains(name,created_at) VALUES ('example.com',0)", Err(UNIQUE, Some("domains_name_key"))));
    v.push(case("domain bad json", "INSERT INTO domains(name,config,created_at) VALUES ('x.org','{not json',0)", Err(BAD_JSON, None)));
    v.push(case("domain good json", "INSERT INTO domains(name,config,created_at) VALUES ('x.org','{\"a\":1}',0)", Pass));
    v.push(case("domain explicit id refused (GENERATED ALWAYS)", "INSERT INTO domains(id,name,created_at) VALUES (50,'y.org',0)", Err(ALWAYS, None)));
    v.push(case("plan explicit id refused", "INSERT INTO plans(id,name) VALUES (50,'p2')", Err(ALWAYS, None)));
    v.push(case("account explicit id refused", "INSERT INTO accounts(id,local_part,domain_id,status,created_at) VALUES (50,'q',1,'active',0)", Err(ALWAYS, None)));
    v.push(case("alias explicit id refused", "INSERT INTO aliases(id,local_part,domain_id,account_id) VALUES (50,'q',1,1)", Err(ALWAYS, None)));
    v.push(case("list explicit id refused", "INSERT INTO lists(id,local_part,domain_id) VALUES (50,'q',1)", Err(ALWAYS, None)));
    v.push(case("mailbox explicit id refused", "INSERT INTO mailboxes(id,account_id,uid_validity) VALUES (50,1,1)", Err(ALWAYS, None)));
    v.push(case("job explicit id refused", &job("id,kind,payload", "50,'k','{}'"), Err(ALWAYS, None)));
    v.push(case("domain delete with accounts", "DELETE FROM domains WHERE id=1", Err(RESTRICT, Some("accounts_domain_id_fkey"))));
    v.push(case("domain delete when empty", "DELETE FROM domains WHERE id=2", Pass));
    v.push(case("plan empty name", "INSERT INTO plans(name) VALUES ('')", Err(CHECK, Some("plans_name_valid"))));
    v.push(case("plan duplicate", "INSERT INTO plans(name) VALUES ('basic')", Err(UNIQUE, Some("plans_name_key"))));
    v.push(case("plan bad json", "INSERT INTO plans(name,config) VALUES ('p','nope')", Err(BAD_JSON, None)));
    v.push(case("plan good json", "INSERT INTO plans(name,config) VALUES ('p','{}')", Pass));

    // ---- accounts
    v.push(case("account missing domain", "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('x',99,'active',0)", Err(FK, Some("address_owners_domain_id_fkey"))));
    v.push(with("account for new domain", &["INSERT INTO domains(id,name,created_at) OVERRIDING SYSTEM VALUE VALUES (99,'x.org',0)"], "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',99,'active',0)", Pass));
    v.push(case("account missing plan", "INSERT INTO accounts(local_part,domain_id,plan_id,status,created_at) VALUES ('x',1,77,'active',0)", Err(FK, Some("accounts_plan_id_fkey"))));
    v.push(case("account with plan", "INSERT INTO accounts(local_part,domain_id,plan_id,status,created_at) VALUES ('x',1,1,'active',0)", Pass));
    v.push(with("plan delete while referenced", &["UPDATE accounts SET plan_id=1 WHERE id=1"], "DELETE FROM plans WHERE id=1", Err(RESTRICT, Some("accounts_plan_id_fkey"))));
    v.push(case("account bad status", "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('x',1,'bogus',0)", Err(CHECK, Some("accounts_status_valid"))));
    v.push(case("account uppercase local part", "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('X',1,'active',0)", Err(CHECK, Some("accounts_local_part_valid"))));
    v.push(case("account empty local part", "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('',1,'active',0)", Err(CHECK, Some("accounts_local_part_valid"))));
    v.push(case("account bad json config", "INSERT INTO accounts(local_part,domain_id,status,config,created_at) VALUES ('x',1,'active','{',0)", Err(BAD_JSON, None)));
    v.push(case("account duplicate address", "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',1,'active',0)", Err(RS001, None)));
    v.push(case("account same local part, other domain", "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('a',2,'active',0)", Pass));
    v.push(case("account vs alias", "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('a',1,2)", Err(RS001, None)));
    v.push(with("alias then account of same address", &[ALIAS_AL], "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('al',1,'active',0)", Err(RS001, None)));
    v.push(case("account vs list", "INSERT INTO lists(local_part,domain_id) VALUES ('a',1)", Err(RS001, None)));
    v.push(with("list then account of same address", &[LIST_T], "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('t',1,'active',0)", Err(RS001, None)));
    v.push(case("account update collides with account", "UPDATE accounts SET local_part='b' WHERE id=1", Err(RS001, None)));
    v.push(with("account update collides with list", &[LIST_T], "UPDATE accounts SET local_part='t' WHERE id=1", Err(RS001, None)));
    v.push(with("account update collides with alias", &[ALIAS_AL], "UPDATE accounts SET local_part='al' WHERE id=2", Err(RS001, None)));
    v.push(with("account domain_id update collides", &["INSERT INTO lists(local_part,domain_id) VALUES ('a',2)"], "UPDATE accounts SET domain_id=2 WHERE id=1", Err(RS001, None)));
    v.push(case("account update to free address", "UPDATE accounts SET local_part='fresh' WHERE id=1", Pass));
    v.push(case("account update to same address", "UPDATE accounts SET local_part='a', domain_id=1 WHERE id=1", Pass));
    v.push(with("account update releases old address", &["UPDATE accounts SET local_part='moved' WHERE id=1"], "INSERT INTO lists(local_part,domain_id) VALUES ('a',1)", Pass));
    v.push(with("account update claims new address", &["UPDATE accounts SET local_part='moved' WHERE id=1"], "INSERT INTO lists(local_part,domain_id) VALUES ('moved',1)", Err(RS001, None)));
    v.push(with("account delete releases address", &["DELETE FROM accounts WHERE id=2"], "INSERT INTO lists(local_part,domain_id) VALUES ('b',1)", Pass));

    // ---- aliases
    v.push(case("alias uppercase", "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('AL',1,1)", Err(CHECK, Some("aliases_local_part_valid"))));
    v.push(case("alias empty", "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('',1,1)", Err(CHECK, Some("aliases_local_part_valid"))));
    v.push(case("alias missing account", "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',1,99)", Err(FK, Some("aliases_account_id_fkey"))));
    v.push(case("alias missing domain", "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',99,1)", Err(FK, None)));
    v.push(with("alias duplicate", &[ALIAS_AL], "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',1,2)", Err(RS001, None)));
    v.push(with("alias vs list", &[LIST_T], "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('t',1,1)", Err(RS001, None)));
    v.push(with("alias update collides", &[ALIAS_AL], "UPDATE aliases SET local_part='b' WHERE local_part='al'", Err(RS001, None)));
    v.push(with("alias domain_id update collides", &[ALIAS_AL, "INSERT INTO lists(local_part,domain_id) VALUES ('al',2)"], "UPDATE aliases SET domain_id=2 WHERE local_part='al' AND domain_id=1", Err(RS001, None)));
    v.push(with("alias delete releases address", &[ALIAS_AL, "DELETE FROM aliases WHERE local_part='al'"], "INSERT INTO lists(local_part,domain_id) VALUES ('al',1)", Pass));
    v.push(with("alias cascade on account delete releases address", &["INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',1,2)", "DELETE FROM accounts WHERE id=2"], "INSERT INTO lists(local_part,domain_id) VALUES ('al',1)", Pass));

    // ---- lists / list_members / address_owners
    v.push(case("list uppercase", "INSERT INTO lists(local_part,domain_id) VALUES ('TEAM',1)", Err(CHECK, Some("lists_local_part_valid"))));
    v.push(case("list empty", "INSERT INTO lists(local_part,domain_id) VALUES ('',1)", Err(CHECK, Some("lists_local_part_valid"))));
    v.push(case("list missing domain", "INSERT INTO lists(local_part,domain_id) VALUES ('t',99)", Err(FK, None)));
    v.push(with("list duplicate", &[LIST_T], LIST_T, Err(RS001, None)));
    v.push(with("list update collides", &[LIST_T], "UPDATE lists SET local_part='a' WHERE local_part='t'", Err(RS001, None)));
    v.push(with("list domain_id update collides", &["INSERT INTO lists(local_part,domain_id) VALUES ('a',2)"], "UPDATE lists SET domain_id=1 WHERE local_part='a' AND domain_id=2", Err(RS001, None)));
    v.push(with("list update releases old address", &[LIST_T, "UPDATE lists SET local_part='t2' WHERE local_part='t'"], "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('t',1,1)", Pass));
    v.push(with("list delete releases address", &[LIST_T, "DELETE FROM lists WHERE local_part='t'"], "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('t',1,1)", Pass));
    v.push(with("member with both account and external", &[LIST_T], "INSERT INTO list_members(list_id,account_id,external) SELECT id,1,'x@y.test' FROM lists", Err(CHECK, Some("list_members_exactly_one"))));
    v.push(with("member with neither", &[LIST_T], "INSERT INTO list_members(list_id) SELECT id FROM lists", Err(CHECK, Some("list_members_exactly_one"))));
    v.push(with("member account ok", &[LIST_T], "INSERT INTO list_members(list_id,account_id) SELECT id,1 FROM lists", Pass));
    v.push(with("member duplicate account", &[LIST_T, "INSERT INTO list_members(list_id,account_id) SELECT id,1 FROM lists"], "INSERT INTO list_members(list_id,account_id) SELECT id,1 FROM lists", Err(UNIQUE, Some("list_members_list_id_account_id_key"))));
    v.push(with("member duplicate external", &[LIST_T, "INSERT INTO list_members(list_id,external) SELECT id,'x@y.test' FROM lists"], "INSERT INTO list_members(list_id,external) SELECT id,'x@y.test' FROM lists", Err(UNIQUE, Some("list_members_list_id_external_key"))));
    v.push(case("member missing list", "INSERT INTO list_members(list_id,account_id) VALUES (99999,1)", Err(FK, Some("list_members_list_id_fkey"))));
    v.push(with("member missing account", &[LIST_T], "INSERT INTO list_members(list_id,account_id) SELECT id,99999 FROM lists", Err(FK, Some("list_members_account_id_fkey"))));
    v.push(with("member explicit duplicate seq", &[LIST_T, "INSERT INTO list_members(list_id,external) SELECT id,'a@y.test' FROM lists"], "INSERT INTO list_members(list_id,external,seq) SELECT id,'b@y.test',1 FROM lists", Err(UNIQUE, Some("list_members_seq"))));
    v.push(with("member seq cannot become null", &[LIST_T, "INSERT INTO list_members(list_id,external) SELECT id,'a@y.test' FROM lists"], "UPDATE list_members SET seq=NULL", Err(NOTNULL, None)));
    v.push(case("registry bad owner_kind (direct)", "INSERT INTO address_owners(domain_id,local_part,owner_kind,owner_id) VALUES (1,'zz','robot',1)", Err(RS003, None)));
    v.push(case("registry direct insert refused", "INSERT INTO address_owners(domain_id,local_part,owner_kind,owner_id) VALUES (1,'zz','account',900)", Err(RS003, None)));
    v.push(case("registry direct delete refused", "DELETE FROM address_owners", Err(RS003, None)));
    v.push(case("registry direct update refused", "UPDATE address_owners SET local_part='zz'", Err(RS003, None)));
    v.push(case("registry truncate refused", "TRUNCATE address_owners", Err(RS003, None)));

    // ---- mailboxes
    v.push(case("mailbox duplicate account", "INSERT INTO mailboxes(account_id,uid_validity) VALUES (1,1)", Err(UNIQUE, Some("mailboxes_account_id_key"))));
    v.push(case("mailbox missing account", "INSERT INTO mailboxes(account_id,uid_validity) VALUES (99,1)", Err(FK, Some("mailboxes_account_id_fkey"))));
    for (col, name) in [("next_msg_id", "mailboxes_next_msg_id_valid"), ("next_change_seq", "mailboxes_next_change_seq_valid"), ("next_folder_id", "mailboxes_next_folder_id_valid")] {
        v.push(case(&format!("mailbox {col}=0"), &format!("UPDATE mailboxes SET {col}=0 WHERE id=1"), Err(CHECK, Some(name))));
        v.push(case(&format!("mailbox {col}=1"), &format!("UPDATE mailboxes SET {col}=1 WHERE id=1"), Pass));
    }
    v.push(case("mailbox uid_validity=0", "UPDATE mailboxes SET uid_validity=0 WHERE id=1", Err(CHECK, Some("mailboxes_uid_validity_valid"))));

    // ---- folders
    v.push(case("folder id 0", &fol(1, 0, "NULL", "Z", "custom"), Err(CHECK, Some("folders_id_valid"))));
    v.push(case("folder empty name", &fol(1, 2, "NULL", "", "custom"), Err(CHECK, Some("folders_name_valid"))));
    v.push(case("folder name with slash", &fol(1, 2, "NULL", "a/b", "custom"), Err(CHECK, Some("folders_name_valid"))));
    v.push(case("folder bad role", &fol(1, 2, "NULL", "Z", "weird"), Err(CHECK, Some("folders_role_valid"))));
    v.push(case("folder uid_validity 0", "INSERT INTO folders(mailbox_id,id,name,uid_validity) VALUES (1,2,'Z',0)", Err(CHECK, Some("folders_uid_validity_valid"))));
    v.push(case("folder next_uid 0", "INSERT INTO folders(mailbox_id,id,name,uid_validity,next_uid) VALUES (1,2,'Z',1,0)", Err(CHECK, Some("folders_next_uid_valid"))));
    v.push(case("folder missing mailbox", &fol(99, 1, "NULL", "Z", "custom"), Err(FK, Some("folders_mailbox_id_fkey"))));
    v.push(case("folder missing parent", &fol(1, 2, "77", "Z", "custom"), Err(FK, Some("folders_mailbox_id_parent_id_fkey"))));
    v.push(with("folder parent in other mailbox", &[&fol(2, 5, "NULL", "Other", "custom")], &fol(1, 6, "5", "Z", "custom"), Err(FK, Some("folders_mailbox_id_parent_id_fkey"))));
    v.push(case("folder parent equal self (insert)", &fol(1, 2, "2", "Z", "custom"), Err(CHECK, Some("folders_parent_not_self"))));
    v.push(case("folder duplicate id", &fol(1, 1, "NULL", "Z", "custom"), Err(UNIQUE, Some("folders_pkey"))));
    v.push(with("folder duplicate sibling name", &[&fol(1, 2, "1", "Work", "custom")], &fol(1, 3, "1", "Work", "custom"), Err(UNIQUE, Some("folders_sibling_name"))));
    v.push(case("folder duplicate root name", &fol(1, 2, "NULL", "INBOX", "custom"), Err(UNIQUE, Some("folders_sibling_name"))));
    v.push(with("folder same name different parent", &[&fol(1, 2, "1", "Work", "custom"), &fol(1, 3, "NULL", "Other", "custom")], &fol(1, 4, "3", "Work", "custom"), Pass));
    v.push(case("folder same name other mailbox", &fol(2, 2, "NULL", "Mine", "custom"), Pass));
    v.push(with("two trash folders", &[&fol(1, 2, "NULL", "T1", "trash")], &fol(1, 3, "NULL", "T2", "trash"), Err(UNIQUE, Some("folders_role"))));
    v.push(with("trash in two mailboxes", &[&fol(1, 2, "NULL", "T1", "trash")], &fol(2, 2, "NULL", "T1", "trash"), Pass));
    v.push(with("many custom folders", &[&fol(1, 2, "NULL", "C1", "custom")], &fol(1, 3, "NULL", "C2", "custom"), Pass));
    v.push(with("folder cycle of three", &[&fol(1, 2, "1", "A", "custom"), &fol(1, 3, "2", "B", "custom"), &fol(1, 4, "3", "C", "custom")], "UPDATE folders SET parent_id=4 WHERE mailbox_id=1 AND id=2", Err(RS002, None)));
    v.push(with("folder reparent after detaching", &[&fol(1, 2, "1", "A", "custom"), &fol(1, 3, "2", "B", "custom"), &fol(1, 4, "3", "C", "custom"), "UPDATE folders SET parent_id=NULL WHERE mailbox_id=1 AND id=3"], "UPDATE folders SET parent_id=4 WHERE mailbox_id=1 AND id=2", Pass));
    v.push(with("folder reparent to self", &[&fol(1, 2, "1", "A", "custom")], "UPDATE folders SET parent_id=2 WHERE mailbox_id=1 AND id=2", Err(RS002, None)));
    v.push(with("long cycle (30 levels) is found", &[CHAIN_30], "UPDATE folders SET parent_id=30 WHERE mailbox_id=1 AND id=1", Err(RS002, None)));
    v.push(with("depth 64 is allowed", &[CHAIN_64, FREE_100], "UPDATE folders SET parent_id=63 WHERE mailbox_id=1 AND id=100", Pass));
    v.push(with("depth 65 is rejected", &[CHAIN_64, FREE_100], "UPDATE folders SET parent_id=64 WHERE mailbox_id=1 AND id=100", Err(RS002, None)));
    v.push(with("deeper than 64 (70 levels) rejected", &[CHAIN_70, FREE_100], "UPDATE folders SET parent_id=70 WHERE mailbox_id=1 AND id=100", Err(RS002, None)));
    v.push(with("shallow reparent beside a deep tree", &[CHAIN_70, FREE_100], "UPDATE folders SET parent_id=2 WHERE mailbox_id=1 AND id=100", Pass));
    // The walk must stay inside the mailbox: mailbox 2 has 2 -> 1; 1 -> 2 is legal in mailbox 1.
    v.push(with("cycle walk is scoped to the mailbox", &[&fol(1, 2, "NULL", "A", "custom"), &fol(2, 2, "1", "A", "custom")], "UPDATE folders SET parent_id=2 WHERE mailbox_id=1 AND id=1", Pass));
    v.push(with("depth walk is scoped to the mailbox", &[CHAIN_70_MB2, &fol(1, 2, "NULL", "A", "custom"), &fol(1, 3, "NULL", "B", "custom")], "UPDATE folders SET parent_id=2 WHERE mailbox_id=1 AND id=3", Pass));
    v.push(with("folder delete with children", &[&fol(1, 2, "1", "Child", "custom")], "DELETE FROM folders WHERE mailbox_id=1 AND id=1", Err(FK, Some("folders_mailbox_id_parent_id_fkey"))));
    v.push(with("folder delete leaf then parent", &[&fol(1, 2, "1", "Child", "custom"), "DELETE FROM folders WHERE mailbox_id=1 AND id=2"], "DELETE FROM folders WHERE mailbox_id=1 AND id=1", Pass));
    v.push(with("folder delete with messages", &[&fol(1, 2, "NULL", "Work", "custom"), &msg(1, 1, 2, 1, A, 5, 0)], "DELETE FROM folders WHERE mailbox_id=1 AND id=2", Err(FK, Some("messages_mailbox_id_folder_id_fkey"))));
    v.push(with("folder delete after messages", &[&fol(1, 2, "NULL", "Work", "custom"), &msg(1, 1, 2, 1, A, 5, 0), "DELETE FROM messages WHERE mailbox_id=1 AND id=1"], "DELETE FROM folders WHERE mailbox_id=1 AND id=2", Pass));

    // ---- blobs
    v.push(case("blob short hash", "INSERT INTO blobs(hash,size) VALUES ('abc'::bytea,1)", Err(CHECK, Some("blobs_hash_length"))));
    v.push(case("blob 33-byte hash", "INSERT INTO blobs(hash,size) VALUES (decode(repeat('cc',33),'hex'),1)", Err(CHECK, Some("blobs_hash_length"))));
    v.push(case("blob 32-byte hash", "INSERT INTO blobs(hash,size) VALUES (decode(repeat('cc',32),'hex'),1)", Pass));
    v.push(case("blob duplicate hash", &format!("INSERT INTO blobs(hash,size) VALUES ({A},1)"), Err(UNIQUE, Some("blobs_pkey"))));
    v.push(case("blob negative size", "INSERT INTO blobs(hash,size) VALUES (decode(repeat('cc',32),'hex'),-1)", Err(CHECK, Some("blobs_size_valid"))));
    v.push(case("blob insert negative refcount", "INSERT INTO blobs(hash,size,refcount) VALUES (decode(repeat('cc',32),'hex'),1,-1)", Err(CHECK, Some("blobs_refcount_valid"))));
    v.push(case("blob update negative refcount", &format!("UPDATE blobs SET refcount=-1 WHERE hash={A}"), Err(CHECK, Some("blobs_refcount_valid"))));
    v.push(case("blob negative zero_since", &format!("UPDATE blobs SET zero_since=-1 WHERE hash={A}"), Err(CHECK, Some("blobs_zero_since_valid"))));
    v.push(case("blob zero_since 0 on unreferenced", &format!("UPDATE blobs SET zero_since=0 WHERE hash={A}"), Pass));
    v.push(case("blob zero_since NULL on unreferenced", &format!("UPDATE blobs SET zero_since=NULL WHERE hash={A}"), Err(CHECK, Some("blobs_zero_since_consistent"))));
    v.push(case("blob referenced with zero_since", "INSERT INTO blobs(hash,size,refcount,zero_since) VALUES (decode(repeat('cc',32),'hex'),1,1,5)", Err(CHECK, Some("blobs_zero_since_consistent"))));
    v.push(case("blob referenced insert", "INSERT INTO blobs(hash,size,refcount) VALUES (decode(repeat('cc',32),'hex'),1,1)", Pass));

    // ---- messages
    v.push(case("message ok", &msg(1, 1, 1, 1, A, 10, 31), Pass));
    v.push(case("message size 0", &msg(1, 1, 1, 1, A, 0, 0), Err(CHECK, Some("messages_size_valid"))));
    v.push(case("message flags 32", &msg(1, 1, 1, 1, A, 10, 32), Err(CHECK, Some("messages_flags_valid"))));
    v.push(case("message flags -1", &msg(1, 1, 1, 1, A, 10, -1), Err(CHECK, Some("messages_flags_valid"))));
    v.push(case("message uid 0", &msg(1, 1, 1, 0, A, 10, 0), Err(CHECK, Some("messages_uid_valid"))));
    v.push(case("message folder missing", &msg(1, 1, 7, 1, A, 10, 0), Err(FK, Some("messages_mailbox_id_folder_id_fkey"))));
    v.push(with("message folder of another mailbox", &[&fol(2, 7, "NULL", "O", "custom")], &msg(1, 1, 7, 1, A, 10, 0), Err(FK, Some("messages_mailbox_id_folder_id_fkey"))));
    v.push(case("message missing mailbox", &msg(99, 1, 1, 1, A, 10, 0), Err(FK, Some("messages_mailbox_id_fkey"))));
    v.push(case("message missing blob", &msg(1, 1, 1, 1, "decode(repeat('dd',32),'hex')", 10, 0), Err(FK, Some("messages_blob_hash_fkey"))));
    v.push(with("message duplicate uid in folder", &[&msg(1, 1, 1, 1, A, 10, 0)], &msg(1, 2, 1, 1, A, 10, 0), Err(UNIQUE, Some("messages_mailbox_id_folder_id_uid_key"))));
    v.push(with("message duplicate id", &[&msg(1, 1, 1, 1, A, 10, 0)], &msg(1, 1, 1, 2, A, 10, 0), Err(UNIQUE, Some("messages_pkey"))));
    v.push(case("message null subject", "INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id,subject) VALUES (1,1,1,1,decode(repeat('aa',32),'hex'),1,0,1,NULL)", Err(NOTNULL, None)));
    v.push(with("blob delete while referenced", &[&msg(1, 1, 1, 1, A, 10, 0)], &format!("DELETE FROM blobs WHERE hash={A}"), Err(RESTRICT, Some("messages_blob_hash_fkey"))));
    v.push(with("blob delete once unreferenced", &[&msg(1, 1, 1, 1, A, 10, 0), "DELETE FROM messages"], &format!("DELETE FROM blobs WHERE hash={A}"), Pass));

    // ---- keywords
    let m1 = msg(1, 1, 1, 1, A, 5, 0);
    v.push(with("keyword uppercase", &[&m1], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'Urgent')", Err(CHECK, Some("keywords_value_valid"))));
    v.push(with("keyword empty", &[&m1], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'')", Err(CHECK, Some("keywords_value_valid"))));
    v.push(with("keyword 256 chars", &[&m1], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,repeat('k',256))", Err(CHECK, Some("keywords_value_valid"))));
    v.push(with("keyword 255 chars", &[&m1], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,repeat('k',255))", Pass));
    v.push(with("keyword ok", &[&m1], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'urgent')", Pass));
    v.push(with("keyword duplicate", &[&m1, "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'urgent')"], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'urgent')", Err(UNIQUE, Some("keywords_pkey"))));
    v.push(case("keyword missing message", "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,9,'urgent')", Err(FK, Some("keywords_mailbox_id_message_id_fkey"))));
    v.push(with("keyword foreign message", &[&m1], "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (2,1,'urgent')", Err(FK, Some("keywords_mailbox_id_message_id_fkey"))));

    // ---- change_log
    v.push(case("change_log ok", "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'message',1,'create')", Pass));
    v.push(case("change_log bad op", "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'message',1,'bogus')", Err(CHECK, Some("change_log_op_valid"))));
    v.push(case("change_log bad kind", "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'weird',1,'create')", Err(CHECK, Some("change_log_kind_valid"))));
    v.push(case("change_log seq 0", "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,0,'message',1,'create')", Err(CHECK, Some("change_log_seq_valid"))));
    v.push(case("change_log missing mailbox", "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (99,1,'message',1,'create')", Err(FK, Some("change_log_mailbox_id_fkey"))));
    v.push(with("change_log duplicate", &["INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'message',1,'create')"], "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'folder',2,'update')", Err(UNIQUE, Some("change_log_pkey"))));

    // ---- deliveries
    v.push(with("delivery ok", &[&m1], &delivery(1, D1, 1, A), Pass));
    v.push(with("delivery duplicate key", &[&m1, &delivery(1, D1, 1, A)], &delivery(1, D1, 1, A), Err(UNIQUE, Some("deliveries_pkey"))));
    v.push(with("delivery second id", &[&m1, &delivery(1, D1, 1, A)], &delivery(1, "decode(repeat('02',32),'hex')", 1, A), Pass));
    v.push(with("delivery short id", &[&m1], &delivery(1, "decode('0101','hex')", 1, A), Err(CHECK, Some("deliveries_id_length"))));
    v.push(with("delivery 33-byte id", &[&m1], &delivery(1, "decode(repeat('01',33),'hex')", 1, A), Err(CHECK, Some("deliveries_id_length"))));
    v.push(with("delivery short hash", &[&m1], &delivery(1, D1, 1, "'ab'::bytea"), Err(CHECK, Some("deliveries_hash_length"))));
    v.push(with("delivery missing message", &[&m1], &delivery(1, D1, 99, A), Err(FK, Some("deliveries_mailbox_id_message_id_fkey"))));
    v.push(with("delivery foreign mailbox", &[&m1], &delivery(2, D1, 1, A), Err(FK, Some("deliveries_mailbox_id_message_id_fkey"))));
    v.push(with("delivery missing mailbox", &[&m1], &delivery(3, D1, 1, A), Err(FK, Some("deliveries_mailbox_id_message_id_fkey"))));

    // ---- jobs
    v.push(case("job ok (defaults)", &job("kind,payload", "'mail.send','{}'"), Pass));
    v.push(case("job kind uppercase", &job("kind,payload", "'UP','{}'"), Err(CHECK, Some("jobs_kind_valid"))));
    v.push(case("job kind empty", &job("kind,payload", "'','{}'"), Err(CHECK, Some("jobs_kind_valid"))));
    v.push(case("job kind trailing newline", &job("kind,payload", "E'ok\\n','{}'"), Err(CHECK, Some("jobs_kind_valid"))));
    v.push(case("job kind 65 chars", &job("kind,payload", "repeat('k',65),'{}'"), Err(CHECK, Some("jobs_kind_valid"))));
    v.push(case("job kind 64 chars", &job("kind,payload", "repeat('k',64),'{}'"), Pass));
    v.push(case("job state bogus", &job("kind,payload,state", "'k','{}','bogus'"), Err(CHECK, Some("jobs_state_valid"))));
    v.push(case("job payload not json", &job("kind,payload", "'k','{'"), Err(CHECK, Some("jobs_payload_valid"))));
    v.push(case("job payload exactly 64KiB", &job("kind,payload", "'k','\"'||repeat('x',65534)||'\"'"), Pass));
    v.push(case("job payload over 64KiB", &job("kind,payload", "'k','\"'||repeat('x',65535)||'\"'"), Err(CHECK, Some("jobs_payload_valid"))));
    v.push(case("job attempts negative", &job("kind,payload,attempts", "'k','{}',-1"), Err(CHECK, Some("jobs_attempts_valid"))));
    v.push(case("job max_attempts 0", &job("kind,payload,max_attempts", "'k','{}',0"), Err(CHECK, Some("jobs_max_attempts_valid"))));
    v.push(case("job max_attempts 21", &job("kind,payload,max_attempts", "'k','{}',21"), Err(CHECK, Some("jobs_max_attempts_valid"))));
    v.push(case("job max_attempts 20", &job("kind,payload,max_attempts", "'k','{}',20"), Pass));
    v.push(case("job attempts above max", &job("kind,payload,attempts,max_attempts", "'k','{}',2,1"), Err(CHECK, Some("jobs_attempts_within_max"))));
    v.push(case("job running without lease", &job("kind,payload,state", "'k','{}','running'"), Err(CHECK, Some("jobs_lease_shape"))));
    v.push(case("job running with lease", &job("kind,payload,state,lease_owner,lease_expires_at", "'k','{}','running','w',10"), Pass));
    v.push(case("job queued with lease", &job("kind,payload,lease_owner,lease_expires_at", "'k','{}','w',10"), Err(CHECK, Some("jobs_lease_shape"))));
    v.push(case("job lease owner empty", &job("kind,payload,state,lease_owner,lease_expires_at", "'k','{}','running','',10"), Err(CHECK, Some("jobs_lease_owner_valid"))));
    v.push(case("job lease owner 129", &job("kind,payload,state,lease_owner,lease_expires_at", "'k','{}','running',repeat('w',129),10"), Err(CHECK, Some("jobs_lease_owner_valid"))));
    v.push(case("job epoch negative", &job("kind,payload,epoch", "'k','{}',-1"), Err(CHECK, Some("jobs_epoch_valid"))));
    v.push(case("job last_error 513 bytes", &job("kind,payload,last_error", "'k','{}',repeat('e',513)"), Err(CHECK, Some("jobs_last_error_valid"))));
    v.push(case("job last_error 512 bytes", &job("kind,payload,last_error", "'k','{}',repeat('e',512)"), Pass));
    v.push(case("job missing domain", &job("kind,payload,domain_id", "'k','{}',99"), Err(FK, Some("jobs_domain_id_fkey"))));
    v.push(with("job blocks domain delete", &[&job("kind,payload,domain_id", "'k','{}',2")], "DELETE FROM domains WHERE id=2", Err(RESTRICT, Some("jobs_domain_id_fkey"))));
    v.push(case("job without run_after", "INSERT INTO jobs(kind,payload,created_at,updated_at) VALUES ('k','{}',0,0)", Err(NOTNULL, None)));

    // ---- capabilities
    let h = |b: u8| format!("decode(repeat('{b:02x}',32),'hex')");
    v.push(case("capability ok", &cap(&h(1), 1, 1, "imap", 100, 200), Pass));
    v.push(case("capability account in other domain", &cap(&h(1), 1, 2, "imap", 100, 200), Err(FK, Some("capabilities_account_id_domain_id_fkey"))));
    v.push(case("capability missing account", &cap(&h(1), 9, 1, "imap", 100, 200), Err(FK, Some("capabilities_account_id_domain_id_fkey"))));
    v.push(case("capability 31-byte hash", &cap("decode(repeat('01',31),'hex')", 1, 1, "imap", 100, 200), Err(CHECK, Some("capabilities_hash_length"))));
    v.push(case("capability 33-byte hash", &cap("decode(repeat('01',33),'hex')", 1, 1, "imap", 100, 200), Err(CHECK, Some("capabilities_hash_length"))));
    v.push(case("capability role admin", &cap(&h(1), 1, 1, "admin", 100, 200), Err(CHECK, Some("capabilities_role_valid"))));
    v.push(case("capability role IMAP", &cap(&h(1), 1, 1, "IMAP", 100, 200), Err(CHECK, Some("capabilities_role_valid"))));
    for r in ["imap", "jmap", "dav", "smtp-submit"] {
        v.push(case(&format!("capability role {r}"), &cap(&h(1), 1, 1, r, 100, 200), Pass));
    }
    v.push(case("capability expiry == issue", &cap(&h(1), 1, 1, "imap", 100, 100), Err(CHECK, Some("capabilities_expiry_valid"))));
    v.push(case("capability expiry < issue", &cap(&h(1), 1, 1, "imap", 100, 99), Err(CHECK, Some("capabilities_expiry_valid"))));
    v.push(case("capability lifetime 86401", &cap(&h(1), 1, 1, "imap", 100, 100 + 86401), Err(CHECK, Some("capabilities_expiry_valid"))));
    v.push(case("capability lifetime 86400", &cap(&h(1), 1, 1, "imap", 100, 100 + 86400), Pass));
    let c1 = cap(&h(1), 1, 1, "imap", 100, 200);
    v.push(with("capability revoked before issue", &[&c1], "UPDATE capabilities SET revoked_at=50", Err(CHECK, Some("capabilities_revoked_valid"))));
    v.push(with("capability revoked at issue", &[&c1], "UPDATE capabilities SET revoked_at=100", Pass));
    v.push(with("capability revoked later", &[&c1], "UPDATE capabilities SET revoked_at=150", Pass));
    v.push(with("capability duplicate hash", &[&c1], &cap(&h(1), 1, 1, "imap", 100, 200), Err(UNIQUE, Some("capabilities_pkey"))));

    v
}

async fn run_case(pool: &PgPool, c: &Case) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for s in &c.setup {
        if let Result::Err(e) = sqlx::query(s).execute(&mut *tx).await {
            return Result::Err(format!("setup failed `{s}`: {e}"));
        }
    }
    let result = sqlx::query(&c.sql).execute(&mut *tx).await;
    let _ = tx.rollback().await;
    match (c.exp, result) {
        (Exp::Ok, Ok(_)) => Ok(()),
        (Exp::Ok, Result::Err(e)) => Result::Err(format!("expected success, got {e}")),
        (Exp::Err(code, _), Ok(_)) => Result::Err(format!("accepted; expected {code}")),
        (Exp::Err(code, constraint), Result::Err(e)) => {
            let db = e.as_database_error();
            let got_code = db.and_then(|d| d.code().map(|c| c.into_owned()));
            let got_constraint = db.and_then(|d| d.constraint().map(str::to_owned));
            if got_code.as_deref() != Some(code) {
                return Result::Err(format!("expected {code}, got {got_code:?}: {e}"));
            }
            if let Some(want) = constraint {
                if got_constraint.as_deref() != Some(want) {
                    return Result::Err(format!(
                        "expected constraint {want}, got {got_constraint:?}: {e}"
                    ));
                }
            }
            Ok(())
        }
    }
}

#[tokio::test]
async fn schema_cases_match_expected_sqlstate_and_constraint() {
    let Some(url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let pool = store.pool();
    seed(pool).await;
    let all = cases();
    let total = all.len();
    let mut failures = vec![];
    for c in &all {
        if let Result::Err(msg) = run_case(pool, c).await {
            failures.push(format!("  [{}] {msg}", c.name));
        }
    }
    store.close().await;
    db.cleanup().await;
    assert!(
        failures.is_empty(),
        "{} of {total} schema cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(total >= 200, "case table shrank unexpectedly: {total}");
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn zero_since(p: &PgPool, hash: &str) -> Option<i64> {
    sqlx::query_scalar(&format!("SELECT zero_since FROM blobs WHERE hash={hash}"))
        .fetch_one(p)
        .await
        .unwrap()
}

async fn count(p: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(p).await.unwrap()
}

async fn exec(p: &PgPool, sql: &str) {
    sqlx::query(sql).execute(p).await.expect(sql);
}

/// Wall-clock window check: kills rs_now() returning milliseconds or a constant.
fn assert_recent(z: i64, before: i64, what: &str) {
    let after = unix_now();
    assert!(
        (before - 2..=after + 2).contains(&z),
        "{what}: zero_since {z} not within [{before}, {after}]"
    );
}

#[tokio::test]
async fn refcount_and_zero_since_follow_message_lifecycle() {
    let Some(url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let p = store.pool();
    let before = unix_now();
    seed(p).await;
    assert_recent(zero_since(p, A).await.expect("set on insert"), before, "insert");

    exec(p, &msg(1, 1, 1, 1, A, 5, 0)).await;
    assert_eq!(zero_since(p, A).await, None);
    exec(p, &msg(1, 2, 1, 2, A, 5, 0)).await;
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash={A}")).await, 2);
    exec(p, "DELETE FROM messages WHERE mailbox_id=1 AND id=2").await;
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash={A}")).await, 1);
    assert_eq!(zero_since(p, A).await, None, "still referenced");

    // moving the last reference A -> B
    exec(p, &format!("UPDATE blobs SET zero_since=1 WHERE hash={B}")).await;
    let before = unix_now();
    exec(p, &format!("UPDATE messages SET blob_hash={B} WHERE mailbox_id=1 AND id=1")).await;
    assert_recent(zero_since(p, A).await.expect("released"), before, "update release");
    assert_eq!(zero_since(p, B).await, None);
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash={B}")).await, 1);

    // deleting the last reference releases it; the unrelated blob keeps its value
    exec(p, &format!("UPDATE blobs SET zero_since=1 WHERE hash={A}")).await;
    let before = unix_now();
    exec(p, "DELETE FROM messages WHERE mailbox_id=1 AND id=1").await;
    assert_recent(zero_since(p, B).await.expect("released"), before, "delete release");
    assert_eq!(zero_since(p, A).await, Some(1));
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash={B}")).await, 0);

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn cascades_release_everything_and_other_tenants_are_untouched() {
    let Some(url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let p = store.pool();
    seed(p).await;
    exec(p, &fol(1, 2, "1", "L2", "custom")).await;
    exec(p, &fol(1, 3, "2", "L3", "custom")).await;
    exec(p, &msg(1, 1, 1, 1, A, 5, 0)).await;
    exec(p, &msg(1, 2, 2, 1, A, 5, 0)).await;
    exec(p, &msg(1, 3, 3, 1, B, 5, 0)).await;
    exec(p, &msg(2, 1, 1, 1, A, 5, 0)).await;
    exec(p, "INSERT INTO keywords(mailbox_id,message_id,keyword) VALUES (1,1,'k'),(2,1,'k')").await;
    exec(p, "INSERT INTO change_log(mailbox_id,seq,kind,item_id,op) VALUES (1,1,'message',1,'create'),(2,1,'message',1,'create')").await;
    exec(p, &delivery(1, D1, 1, A)).await;
    exec(p, &delivery(2, D1, 1, A)).await;
    exec(p, &cap("decode(repeat('01',32),'hex')", 1, 1, "imap", 100, 200)).await;
    exec(p, ALIAS_AL).await;
    exec(p, LIST_T).await;
    exec(p, "INSERT INTO list_members(list_id,account_id) SELECT id,1 FROM lists").await;
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash={A}")).await, 3);

    // deleting one message cascades its keywords and deliveries only
    exec(p, "DELETE FROM messages WHERE mailbox_id=2 AND id=1").await;
    assert_eq!(count(p, "SELECT count(*) FROM keywords WHERE mailbox_id=2").await, 0);
    assert_eq!(count(p, "SELECT count(*) FROM deliveries WHERE mailbox_id=2").await, 0);
    assert_eq!(count(p, &format!("SELECT refcount FROM blobs WHERE hash={A}")).await, 2);

    let before = unix_now();
    exec(p, "DELETE FROM accounts WHERE id=1").await;
    for table in [
        "mailboxes WHERE id=1",
        "folders WHERE mailbox_id=1",
        "messages WHERE mailbox_id=1",
        "keywords WHERE mailbox_id=1",
        "change_log WHERE mailbox_id=1",
        "deliveries WHERE mailbox_id=1",
        "capabilities",
        "aliases",
        "list_members",
    ] {
        assert_eq!(count(p, &format!("SELECT count(*) FROM {table}")).await, 0, "{table}");
    }
    assert_eq!(count(p, "SELECT coalesce(sum(refcount),0)::bigint FROM blobs").await, 0);
    assert_recent(zero_since(p, A).await.unwrap(), before, "cascade release");
    assert_eq!(count(p, "SELECT count(*) FROM folders WHERE mailbox_id=2").await, 1);
    assert_eq!(count(p, "SELECT count(*) FROM change_log WHERE mailbox_id=2").await, 1);
    assert_eq!(count(p, "SELECT count(*) FROM lists").await, 1);

    // deleting a list cascades its members and frees its address
    exec(p, "INSERT INTO list_members(list_id,external) SELECT id,'x@y.test' FROM lists").await;
    exec(p, "DELETE FROM lists").await;
    assert_eq!(count(p, "SELECT count(*) FROM list_members").await, 0);
    exec(p, LIST_T).await;

    // deleting a mailbox cascades deliveries
    exec(p, &msg(2, 5, 1, 5, B, 5, 0)).await;
    exec(p, &delivery(2, D1, 5, B)).await;
    exec(p, "DELETE FROM mailboxes WHERE id=2").await;
    assert_eq!(count(p, "SELECT count(*) FROM deliveries").await, 0);

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn address_registry_never_drifts_from_the_owner_tables() {
    let Some(url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let p = store.pool();
    seed(p).await;
    exec(p, "INSERT INTO aliases(local_part,domain_id,account_id) VALUES ('al',1,1),('al2',1,2)").await;
    exec(p, "INSERT INTO lists(local_part,domain_id) VALUES ('t',1),('t2',2)").await;
    exec(p, "UPDATE aliases SET local_part='al3' WHERE local_part='al2'").await;
    exec(p, "DELETE FROM lists WHERE local_part='t2'").await;
    let owners = "(SELECT domain_id, local_part, 'account' k, id FROM accounts \
         UNION ALL SELECT domain_id, local_part, 'alias', id FROM aliases \
         UNION ALL SELECT domain_id, local_part, 'list', id FROM lists)";
    let registry = "(SELECT domain_id, local_part, owner_kind::text, owner_id FROM address_owners)";
    let drift = format!("SELECT count(*) FROM ({owners} EXCEPT {registry}) a");
    let reverse = format!("SELECT count(*) FROM ({registry} EXCEPT {owners}) a");
    assert_eq!(count(p, &drift).await, 0);
    assert_eq!(count(p, &reverse).await, 0);
    assert_eq!(count(p, "SELECT count(*) FROM address_owners").await, 5);

    // TRUNCATE of an owner table clears that kind's registry rows (no stuck addresses).
    exec(p, "TRUNCATE lists CASCADE").await;
    assert_eq!(count(p, &drift).await, 0);
    assert_eq!(count(p, &reverse).await, 0);
    exec(p, "INSERT INTO accounts(local_part,domain_id,status,created_at) VALUES ('t',1,'active',0)").await;

    store.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn text_keys_use_the_c_collation_and_defaults_hold() {
    let Some(url) = pg_admin_url() else {
        return;
    };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let p = store.pool();
    seed(p).await;
    // Under en_US 'a@x' sorts before 'B@x'; under "C" the uppercase letter comes first.
    exec(p, LIST_T).await;
    exec(p, "INSERT INTO list_members(list_id,external) SELECT id,'a@x' FROM lists").await;
    exec(p, "INSERT INTO list_members(list_id,external) SELECT id,'B@x' FROM lists").await;
    let order: Vec<String> =
        sqlx::query_scalar("SELECT external FROM list_members ORDER BY external")
            .fetch_all(p)
            .await
            .unwrap();
    assert_eq!(order, ["B@x", "a@x"]);

    // Every text column the schema declares COLLATE "C" really has it.
    let non_c: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname || '.' || a.attname FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname='public' AND c.relkind='r' AND a.attnum > 0 AND NOT a.attisdropped \
           AND c.relname || '.' || a.attname IN ('domains.name','plans.name','accounts.local_part', \
               'aliases.local_part','lists.local_part','list_members.external', \
               'address_owners.local_part','folders.name','keywords.keyword','jobs.kind', \
               'messages.message_id_hdr') \
           AND a.attcollation <> (SELECT oid FROM pg_collation WHERE collname='C' LIMIT 1)",
    )
    .fetch_all(p)
    .await
    .unwrap();
    assert!(non_c.is_empty(), "not COLLATE C: {non_c:?}");

    // Column defaults.
    exec(p, "INSERT INTO jobs(kind,payload,run_after,created_at,updated_at) VALUES ('k','{}',0,0,0)").await;
    let job: (String, i64, i64, i64) =
        sqlx::query_as("SELECT state,attempts,max_attempts,epoch FROM jobs")
            .fetch_one(p)
            .await
            .unwrap();
    assert_eq!(job, ("queued".into(), 0, 5, 0));
    exec(p, "INSERT INTO folders(mailbox_id,id,name,uid_validity) VALUES (1,2,'Plain',1)").await;
    let f: (String, i64) =
        sqlx::query_as("SELECT role,next_uid FROM folders WHERE id=2 AND mailbox_id=1")
            .fetch_one(p)
            .await
            .unwrap();
    assert_eq!(f, ("custom".into(), 1));
    let mb: (i64, i64, i64) = sqlx::query_as(
        "SELECT next_msg_id,next_change_seq,next_folder_id FROM mailboxes WHERE id=1",
    )
    .fetch_one(p)
    .await
    .unwrap();
    assert_eq!(mb, (1, 1, 1));
    exec(
        p,
        &format!("INSERT INTO messages(mailbox_id,id,folder_id,uid,blob_hash,size,internal_date,thread_id) VALUES (1,1,1,1,{A},1,0,1)"),
    )
    .await;
    let m: (i64, String, String, String, Option<i64>) =
        sqlx::query_as("SELECT flags,subject,from_addr,to_addrs,deleted_at FROM messages")
            .fetch_one(p)
            .await
            .unwrap();
    assert_eq!(m, (0, String::new(), String::new(), String::new(), None));

    store.close().await;
    db.cleanup().await;
}
