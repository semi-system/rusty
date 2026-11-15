//! `admin_grants` (admin web design, section 5): who may use the admin web. A grant is one row per account; a domain administrator must be an
//! account of that domain (the table says so for any writer); a grant goes with its account and its domain; "is there a platform administrator"
//! is a database fact.
mod common;

use common::backends::*;
use rusty_core::{AccountId, DomainId, Error};

async fn account(b: &Backend, local: &str, domain: DomainId) -> AccountId {
    b.dir.create_account(local, domain, Some("a-long-enough-pw-1".to_owned()), None).await.unwrap()
}

#[tokio::test]
async fn a_platform_grant_and_a_domain_grant_are_stored_and_replace_each_other() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (d1, d2) = (b.dir.create_domain("one.test").await.unwrap(), b.dir.create_domain("two.test").await.unwrap());
        let (a1, a2) = (account(&b, "root", d1).await, account(&b, "boss", d2).await);
        let s = b.admins();
        assert!(!s.has_platform_admin().await.unwrap(), "{n}: nobody yet");
        assert!(s.get(a1).await.unwrap().is_none(), "{n}");
        let g = s.grant_at(a1, None, 100).await.unwrap();
        assert_eq!((g.account, g.domain, g.created_at), (a1, None, 100), "{n}");
        assert!(s.has_platform_admin().await.unwrap(), "{n}");
        // A domain administrator does not make a platform one.
        let g2 = s.grant_at(a2, Some(d2), 200).await.unwrap();
        assert_eq!(g2.domain, Some(d2), "{n}");
        assert_eq!(s.get(a2).await.unwrap().unwrap(), g2, "{n}");
        // Narrowing replaces: the platform grant becomes a grant of its own domain, and the platform has no administrator left.
        s.grant_at(a1, Some(d1), 300).await.unwrap();
        assert_eq!(s.get(a1).await.unwrap().unwrap().domain, Some(d1), "{n}");
        assert!(!s.has_platform_admin().await.unwrap(), "{n}: only domain administrators remain");
        assert_eq!(s.list(None, 50).await.unwrap().len(), 2, "{n}: one row per account");
    })
    .await;
}

#[tokio::test]
async fn a_domain_administrator_must_belong_to_the_domain_for_every_writer() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (d1, d2) = (b.dir.create_domain("one.test").await.unwrap(), b.dir.create_domain("two.test").await.unwrap());
        let a1 = account(&b, "root", d1).await;
        let s = b.admins();
        assert!(matches!(s.grant(a1, Some(d2)).await, Err(Error::Invalid(_))), "{n}: an account of one.test cannot administer two.test");
        // The table refuses it too, for a writer that is not the store.
        let sql = format!("INSERT INTO admin_grants (account_id, domain_id, created_at) VALUES ({}, {}, 1)", a1.0, d2.0);
        assert!(try_execute(&b, &sql, &[]).await.is_err(), "{n}");
        // Moving a valid grant to a domain the account is not in is refused as well.
        s.grant(a1, Some(d1)).await.unwrap();
        let upd = format!("UPDATE admin_grants SET domain_id = {} WHERE account_id = {}", d2.0, a1.0);
        assert!(try_execute(&b, &upd, &[]).await.is_err(), "{n}");
        assert_eq!(s.get(a1).await.unwrap().unwrap().domain, Some(d1), "{n}: unchanged");
        // A platform administrator may belong to any domain.
        assert!(s.grant(a1, None).await.is_ok(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn a_grant_for_something_that_does_not_exist_is_an_error_and_revoking_is_idempotent() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = b.dir.create_domain("one.test").await.unwrap();
        let a = account(&b, "root", d).await;
        let s = b.admins();
        assert!(s.grant(AccountId(987_654), None).await.is_err(), "{n}: no such account");
        assert!(s.grant(a, Some(DomainId(987_654))).await.is_err(), "{n}: no such domain");
        assert!(s.get(a).await.unwrap().is_none(), "{n}: nothing was stored");
        s.grant(a, None).await.unwrap();
        assert!(s.revoke(a).await.unwrap(), "{n}");
        assert!(!s.revoke(a).await.unwrap(), "{n}: revoking twice is not an error");
        assert!(!s.has_platform_admin().await.unwrap(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn a_grant_goes_with_its_account() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (d1, d2) = (b.dir.create_domain("one.test").await.unwrap(), b.dir.create_domain("two.test").await.unwrap());
        let (a1, a2, a3) = (account(&b, "root", d1).await, account(&b, "boss", d2).await, account(&b, "other", d2).await);
        let s = b.admins();
        s.grant(a1, None).await.unwrap();
        s.grant(a2, Some(d2)).await.unwrap();
        s.grant(a3, Some(d2)).await.unwrap();
        run(&b, &format!("DELETE FROM accounts WHERE id = {}", a1.0)).await;
        assert!(s.get(a1).await.unwrap().is_none() && !s.has_platform_admin().await.unwrap(), "{n}: the account went, so did its grant");
        // A domain cannot be deleted while it has accounts, and its administrators are accounts: deleting them leaves nothing behind.
        run(&b, &format!("DELETE FROM accounts WHERE id IN ({}, {})", a2.0, a3.0)).await;
        assert!(s.list(None, 50).await.unwrap().is_empty(), "{n}: no grant outlives its account");
        run(&b, &format!("DELETE FROM domains WHERE id = {}", d2.0)).await;
    })
    .await;
}

#[tokio::test]
async fn listing_pages_by_account_id() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = b.dir.create_domain("one.test").await.unwrap();
        let s = b.admins();
        let mut ids = Vec::new();
        for i in 0..5 {
            let a = account(&b, &format!("admin{i}"), d).await;
            s.grant(a, None).await.unwrap();
            ids.push(a);
        }
        let first = s.list(None, 2).await.unwrap();
        assert_eq!(first.iter().map(|g| g.account).collect::<Vec<_>>(), ids[..2], "{n}");
        let rest = s.list(Some(first[1].account), 50).await.unwrap();
        assert_eq!(rest.iter().map(|g| g.account).collect::<Vec<_>>(), ids[2..], "{n}");
        assert_eq!(s.list(None, 0).await.unwrap().len(), 1, "{n}: the page size is clamped up");
        assert_eq!(s.list(None, 10_000).await.unwrap().len(), 5, "{n}: and down");
    })
    .await;
}
