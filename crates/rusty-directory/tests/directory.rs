use rusty_core::Error;
use rusty_directory::{Directory, Resolved};
use rusty_store::SqliteStore;

async fn dir() -> Directory {
    Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap())
}

async fn count(d: &Directory, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(d.store().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn create_account_makes_mailbox_and_system_folders() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    let mb = d.mailbox_for(acc).await.unwrap();
    let rows: Vec<(i64, String, String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT id, name, role, parent_id, uid_validity FROM folders WHERE mailbox_id = ? ORDER BY id",
    )
    .bind(mb.0)
    .fetch_all(d.store().pool())
    .await
    .unwrap();
    let got: Vec<(i64, &str, &str)> = rows
        .iter()
        .map(|r| (r.0, r.1.as_str(), r.2.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "Inbox", "inbox"),
            (2, "Sent", "sent"),
            (3, "Drafts", "drafts"),
            (4, "Trash", "trash"),
            (5, "Spam", "spam")
        ]
    );
    assert!(rows.iter().all(|r| r.3.is_none() && r.4 > 0));
    let uv: i64 = sqlx::query_scalar("SELECT uid_validity FROM mailboxes WHERE id = ?")
        .bind(mb.0)
        .fetch_one(d.store().pool())
        .await
        .unwrap();
    assert!(uv > 0);
}

#[tokio::test]
async fn address_is_normalized_to_lowercase() {
    let d = dir().await;
    let dom = d.create_domain("Example.COM").await.unwrap();
    let acc = d.create_account("  Alice ", dom, None, None).await.unwrap();
    assert_eq!(
        d.resolve("ALICE@Example.com").await.unwrap(),
        Resolved::Account(acc)
    );
    assert!(matches!(
        d.create_account("alice", dom, None, None).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn empty_local_part_is_invalid() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    for bad in ["", "  ", "a@b", "a b", "a\tb", "a\u{7}b"] {
        assert!(
            matches!(
                d.create_account(bad, dom, None, None).await,
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(d.create_domain("").await, Err(Error::Invalid(_))));
    assert!(matches!(
        d.create_domain("a@b.com").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.resolve("nodomain").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.resolve("@example.com").await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn alias_cannot_reuse_account_address() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    assert!(matches!(
        d.add_alias("alice", dom, acc).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        d.create_list("alice", dom).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn resolve_alias_returns_account() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    d.add_alias("Ally", dom, acc).await.unwrap();
    assert_eq!(
        d.resolve("ally@example.com").await.unwrap(),
        Resolved::Account(acc)
    );
}

#[tokio::test]
async fn resolve_list_expands_members() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let a = d.create_account("alice", dom, None, None).await.unwrap();
    let list = d.create_list("team", dom).await.unwrap();
    d.add_list_member(list, Some(a), None).await.unwrap();
    d.add_list_member(list, None, Some("Bob@Other.org"))
        .await
        .unwrap();
    assert!(matches!(
        d.add_list_member(list, Some(a), Some("x@y.z")).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.add_list_member(list, None, None).await,
        Err(Error::Invalid(_))
    ));
    let Resolved::List(m) = d.resolve("team@example.com").await.unwrap() else {
        panic!()
    };
    assert_eq!(m.len(), 2);
    assert!(m.contains(&Resolved::Account(a)));
    assert!(m.contains(&Resolved::External("bob@other.org".into())));
}

#[tokio::test]
async fn resolve_unknown_is_not_found() {
    let d = dir().await;
    d.create_domain("example.com").await.unwrap();
    assert!(matches!(
        d.resolve("nobody@example.com").await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        d.resolve("nobody@nowhere.com").await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn create_account_rolls_back_on_failure() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    d.create_account("alice", dom, None, None).await.unwrap();
    assert!(d.create_account("alice", dom, None, None).await.is_err());
    assert!(d
        .create_account("bob", rusty_core::DomainId(999), None, None)
        .await
        .is_err());
    assert_eq!(count(&d, "accounts").await, 1);
    assert_eq!(count(&d, "mailboxes").await, 1);
    assert_eq!(count(&d, "folders").await, 5);
}

async fn domain_name(d: &Directory, id: rusty_core::DomainId) -> String {
    sqlx::query_scalar("SELECT name FROM domains WHERE id = ?")
        .bind(id.0)
        .fetch_one(d.store().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn unicode_domain_stored_as_punycode() {
    let d = dir().await;
    let id = d.create_domain("münchen.de").await.unwrap();
    assert_eq!(domain_name(&d, id).await, "xn--mnchen-3ya.de");
    let id2 = d.create_domain(" EXAMPLE.COM. ").await.unwrap();
    assert_eq!(domain_name(&d, id2).await, "example.com");
}

#[tokio::test]
async fn uppercase_unicode_domain_same_as_lowercase() {
    let d = dir().await;
    d.create_domain("münchen.de").await.unwrap();
    assert!(matches!(
        d.create_domain("MÜNCHEN.DE").await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn nfd_and_nfc_local_part_are_same_account() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let a = d.create_account("\u{e9}", dom, None, None).await.unwrap();
    assert!(matches!(
        d.create_account("e\u{301}", dom, None, None).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        d.resolve("e\u{301}@example.com").await.unwrap(),
        Resolved::Account(a)
    );
}

#[tokio::test]
async fn resolve_accepts_unicode_and_punycode_forms() {
    let d = dir().await;
    let dom = d.create_domain("münchen.de").await.unwrap();
    let a = d.create_account("bob", dom, None, None).await.unwrap();
    for addr in [
        "Bob@München.DE",
        "bob@xn--mnchen-3ya.de",
        "bob@mu\u{308}nchen.de",
    ] {
        assert_eq!(
            d.resolve(addr).await.unwrap(),
            Resolved::Account(a),
            "{addr}"
        );
    }
}

#[tokio::test]
async fn label_longer_than_63_is_invalid() {
    let d = dir().await;
    let long = format!("{}.com", "a".repeat(64));
    assert!(matches!(
        d.create_domain(&long).await,
        Err(Error::Invalid(_))
    ));
    let total = format!("{0}.{0}.{0}.{0}.com", "a".repeat(62));
    assert!(matches!(
        d.create_domain(&total).await,
        Err(Error::Invalid(_))
    ));
    d.create_domain(&format!("{}.com", "a".repeat(63)))
        .await
        .unwrap();
}

#[tokio::test]
async fn local_part_over_64_octets_is_invalid() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    assert!(matches!(
        d.create_account(&"a".repeat(65), dom, None, None).await,
        Err(Error::Invalid(_))
    ));
    d.create_account(&"a".repeat(64), dom, None, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn quoted_or_whitespace_local_part_is_invalid() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    for bad in ["\"a b\"", "a(c)b", "a b", "a\nb", "a\x5cb"] {
        assert!(
            matches!(
                d.create_account(bad, dom, None, None).await,
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn domain_idna_invalid_is_invalid() {
    let d = dir().await;
    for bad in [
        "xn--a.com",
        "a_b.com",
        "exa mple.com",
        "a..com",
        "-.com",
        "a b@c.com",
    ] {
        assert!(
            matches!(d.create_domain(bad).await, Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn display_domain_roundtrip() {
    assert_eq!(
        rusty_directory::display_domain("xn--mnchen-3ya.de"),
        "münchen.de"
    );
    assert_eq!(
        rusty_directory::display_domain("example.com"),
        "example.com"
    );
    let (_, p) = rusty_directory::parse_address("x@münchen.de").unwrap();
    assert_eq!(rusty_directory::display_domain(&p), "münchen.de");
}
