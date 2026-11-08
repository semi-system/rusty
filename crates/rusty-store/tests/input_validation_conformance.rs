//! Repository input validation on BOTH backends (review PG6 finding 4).
//!
//! The Postgres schema has no length checks and rejects NUL bytes in text; SQLite accepts both.
//! Without validation a 4000-character local part, domain or list member address reached the
//! database and came back as `Storage` on Postgres (SQLSTATE 54000, btree row too large) while
//! SQLite stored it, and a NUL byte was `Storage` (22021) versus accepted. The repositories now
//! refuse such values as `Invalid` before any statement runs, identically on both backends, and
//! leave nothing behind.

mod common;

use common::backends::{for_each_backend, Backend};
use rusty_core::Error;
use rusty_store::repo::NewAccount;

macro_rules! invalid {
    ($what:expr, $r:expr) => {
        assert!(matches!($r, Err(Error::Invalid(_))), "{}: {:?}", $what, $r)
    };
}

/// 4000 characters that do not compress (what hits Postgres' btree row limit).
fn random_text(n: usize) -> String {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (b'a' + (x % 26) as u8) as char
        })
        .collect()
}

fn new_account(local: &str, domain: rusty_core::DomainId, hash: Option<&str>) -> NewAccount {
    NewAccount {
        local: local.into(),
        domain,
        plan: None,
        password_hash: hash.map(str::to_owned),
    }
}

async fn count(b: &Backend, table: &str) -> i64 {
    common::backends::scalar(b, &format!("SELECT count(*) FROM {table}")).await
}

#[tokio::test]
async fn oversized_and_nul_text_is_invalid_before_the_database_on_both_backends() {
    for_each_backend(|b| async move {
        let n = b.name;
        let long = random_text(4000);
        let dom = b.drepo.create_domain("valid.test").await.unwrap();
        let acc = b.drepo.create_account(&new_account("alice", dom, None)).await.unwrap();
        let list = b.drepo.create_list("staff", dom).await.unwrap();
        let before = (
            count(&b, "domains").await,
            count(&b, "accounts").await,
            count(&b, "aliases").await,
            count(&b, "lists").await,
            count(&b, "list_members").await,
            count(&b, "plans").await,
        );

        // Domains: at most 253 octets, no NUL.
        invalid!(format!("{n} long domain"), b.drepo.create_domain(&long).await);
        invalid!(format!("{n} 254-octet domain"), b.drepo.create_domain(&"d".repeat(254)).await);
        invalid!(format!("{n} NUL domain"), b.drepo.create_domain("a\0b.test").await);
        // Local parts: at most 64 octets, no NUL.
        for (what, local) in [("long", long.clone()), ("65 octets", "l".repeat(65)), ("NUL", "a\0b".to_owned())] {
            invalid!(format!("{n} account local part {what}"), b.drepo.create_account(&new_account(&local, dom, None)).await);
            invalid!(format!("{n} alias local part {what}"), b.drepo.add_alias(&local, dom, acc).await);
            invalid!(format!("{n} list local part {what}"), b.drepo.create_list(&local, dom).await);
        }
        // List member addresses: at most 320 octets (64 + @ + 255), no NUL.
        for (what, ext) in [("long", long.clone()), ("321 octets", "e".repeat(321)), ("NUL", "a\0b@x.test".to_owned())] {
            invalid!(format!("{n} list member {what}"), b.drepo.add_list_member(list, None, Some(&ext), 100).await);
        }
        // Other text columns: NUL is refused too.
        invalid!(format!("{n} NUL password hash"), b.drepo.create_account(&new_account("bob", dom, Some("h\0x"))).await);
        invalid!(format!("{n} NUL password hash update"), b.drepo.set_password_hash(acc, "h\0x").await);
        invalid!(format!("{n} NUL plan name"), b.drepo.create_plan("p\0q", "{}").await);
        // Lookups of an address that cannot exist are refused alike, not answered with a storage error.
        invalid!(format!("{n} long resolve local part"), b.drepo.resolve(&long, "valid.test", 10).await);
        invalid!(format!("{n} NUL resolve domain"), b.drepo.resolve("alice", "valid\0.test", 10).await);

        let after = (
            count(&b, "domains").await,
            count(&b, "accounts").await,
            count(&b, "aliases").await,
            count(&b, "lists").await,
            count(&b, "list_members").await,
            count(&b, "plans").await,
        );
        assert_eq!(before, after, "{n}: a refused value left rows behind");
    })
    .await;
}

#[tokio::test]
async fn values_at_the_limits_are_accepted_on_both_backends() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.drepo.create_domain(&"d".repeat(253)).await.unwrap_or_else(|e| panic!("{n}: {e:?}"));
        let local = "l".repeat(64);
        let acc = b.drepo.create_account(&new_account(&local, dom, None)).await.unwrap();
        let list = b.drepo.create_list("m".repeat(64).as_str(), dom).await.unwrap();
        b.drepo.add_list_member(list, None, Some(&"e".repeat(320)), 100).await.unwrap();
        b.drepo.add_alias(&"a".repeat(64), dom, acc).await.unwrap();
    })
    .await;
}
