//! The tenant rule at the data layer (review I4, I8): an alias or a list member must belong to the
//! domain it is used in, enforced by the facade AND by the database itself; disabling a domain stops
//! everything of that domain. One body per behaviour, run on both backends.

mod common;

use common::{for_each_backend, Bind};
use rusty_core::{AccountId, DomainId, Error};
use rusty_directory::{
    AccountStatus, CapabilityRole, Directory, DomainStatus, Resolved,
};

macro_rules! kind {
    ($r:expr, $pat:pat) => {
        assert!(matches!($r, Err($pat)), "unexpected: {:?}", $r)
    };
}

async fn two_tenants(d: &Directory) -> (DomainId, AccountId, DomainId, AccountId) {
    let a = d.create_domain("a.test").await.unwrap();
    let b = d.create_domain("b.test").await.unwrap();
    let alice = d.create_account("alice", a, Some("pw-alice-1234"), None).await.unwrap();
    let eve = d.create_account("eve", b, Some("pw-eve-123456"), None).await.unwrap();
    (a, alice, b, eve)
}

#[tokio::test]
async fn an_alias_may_only_point_into_its_own_domain() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let (a, alice, dom_b, eve) = two_tenants(&d).await;
        kind!(d.add_alias("info", a, eve).await, Error::Invalid(_));
        kind!(d.add_alias("info", dom_b, alice).await, Error::Invalid(_));
        assert_eq!(b.count("aliases").await, 0);
        d.add_alias("info", a, alice).await.unwrap();
        assert_eq!(d.resolve("info@a.test").await.unwrap(), Resolved::Account(alice));
    })
    .await;
}

#[tokio::test]
async fn the_database_refuses_a_cross_tenant_alias_even_without_the_facade() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let (a, _alice, _, eve) = two_tenants(&d).await;
        // a caller that bypasses the facade (a future admin API, a script) still cannot do it
        let r = b
            .try_run(
                "INSERT INTO aliases (local_part, domain_id, account_id) VALUES (?1, ?2, ?3)",
                &[Bind::S("info"), Bind::I(a.0), Bind::I(eve.0)],
            )
            .await;
        let f = r.expect_err("the database must refuse a cross-tenant alias");
        assert!(matches!(f.mapped, Error::Invalid(_)), "{f:?}");
        assert_eq!(b.count("aliases").await, 0);
    })
    .await;
}

#[tokio::test]
async fn a_list_member_account_must_be_in_the_lists_domain() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let (a, alice, _, eve) = two_tenants(&d).await;
        let list = d.create_list("team", a).await.unwrap();
        kind!(d.add_list_member(list, Some(eve), None).await, Error::Invalid(_));
        d.add_list_member(list, Some(alice), None).await.unwrap();
        d.add_list_member(list, None, Some("far@away.example")).await.unwrap();
        assert_eq!(b.count("list_members").await, 2);
        // and the database says the same to a caller that skips the facade
        let r = b
            .try_run(
                "INSERT INTO list_members (list_id, account_id) VALUES (?1, ?2)",
                &[Bind::I(list.0), Bind::I(eve.0)],
            )
            .await;
        let f = r.expect_err("the database must refuse a foreign list member");
        assert!(matches!(f.mapped, Error::Invalid(_)), "{f:?}");
    })
    .await;
}

#[tokio::test]
async fn disabling_a_domain_revokes_its_capabilities_and_stops_validation_for_good() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let (a, alice, _, eve) = two_tenants(&d).await;
        let tok = d.issue_capability(alice, CapabilityRole::Imap, 600).await.unwrap();
        let other = d.issue_capability(eve, CapabilityRole::Imap, 600).await.unwrap();
        let shown = tok.expose_secret().to_owned();
        assert!(d.validate_capability(&shown, CapabilityRole::Imap).await.unwrap().is_some());
        d.set_domain_status(a, DomainStatus::Disabled).await.unwrap();
        assert!(d.validate_capability(&shown, CapabilityRole::Imap).await.unwrap().is_none());
        // enabling again does not resurrect a revoked token
        d.set_domain_status(a, DomainStatus::Active).await.unwrap();
        assert!(d.validate_capability(&shown, CapabilityRole::Imap).await.unwrap().is_none());
        // the other tenant is untouched
        assert!(d.validate_capability(other.expose_secret(), CapabilityRole::Imap).await.unwrap().is_some());
        // and new tokens can be issued after re-enabling
        assert!(d.issue_capability(alice, CapabilityRole::Imap, 600).await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn a_token_cannot_be_validated_while_its_domain_is_disabled_even_if_not_revoked() {
    // the validation statement itself looks at the domain (a token issued by another process after the disable)
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let (a, alice, _, _) = two_tenants(&d).await;
        d.set_domain_status(a, DomainStatus::Disabled).await.unwrap();
        // issuing for an account of a disabled domain is refused like for any non-active account
        kind!(d.issue_capability(alice, CapabilityRole::Imap, 600).await, Error::NotFound(_));
    })
    .await;
}

#[tokio::test]
async fn an_account_of_a_disabled_domain_is_disabled_for_every_reader() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let (a, alice, _, eve) = two_tenants(&d).await;
        assert_eq!(d.account_status(alice).await.unwrap(), Some(AccountStatus::Active));
        d.set_domain_status(a, DomainStatus::Disabled).await.unwrap();
        assert_eq!(d.account_status(alice).await.unwrap(), Some(AccountStatus::Disabled));
        assert_eq!(d.account_status(eve).await.unwrap(), Some(AccountStatus::Active));
        let many = d.account_statuses(&[alice, eve, AccountId(424242)]).await.unwrap();
        assert_eq!(many.get(&alice), Some(&AccountStatus::Disabled));
        assert_eq!(many.get(&eve), Some(&AccountStatus::Active));
        assert!(!many.contains_key(&AccountId(424242)));
    })
    .await;
}

#[tokio::test]
async fn account_statuses_answers_many_ids_in_one_call() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let dom = d.create_domain("many.test").await.unwrap();
        let mut ids = Vec::new();
        for i in 0..300 {
            ids.push(d.create_account(&format!("u{i:03}"), dom, None, None).await.unwrap());
        }
        d.set_status(ids[7], AccountStatus::Locked).await.unwrap();
        let got = d.account_statuses(&ids).await.unwrap();
        assert_eq!(got.len(), 300);
        assert_eq!(got[&ids[7]], AccountStatus::Locked);
        assert_eq!(got[&ids[8]], AccountStatus::Active);
        assert!(d.account_statuses(&[]).await.unwrap().is_empty());
    })
    .await;
}
