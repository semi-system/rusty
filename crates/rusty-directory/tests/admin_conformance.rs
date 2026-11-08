//! Administration reads and the domain/alias lifecycle operations added for `rustyctl` (U2):
//! enumeration with keyset paging, exact account lookup, domain enable/disable, alias removal.
//! One body per behaviour, run on SQLite and (feature `postgres` + `RUSTY_TEST_PG_URL`) on
//! Postgres.

mod common;

use common::for_each_backend;
use rusty_core::{AccountId, DomainId, Error};
use rusty_directory::{
    AccountStatus, Authenticator, Directory, DomainStatus, LocalAuthenticator, Resolved,
    MAX_LIST_PAGE,
};

macro_rules! kind {
    ($r:expr, $pat:pat) => {
        assert!(matches!($r, Err($pat)), "unexpected: {:?}", $r)
    };
}

async fn all_domains(d: &Directory) -> Vec<rusty_directory::DomainInfo> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = d.list_domains(after, 2).await.unwrap();
        if page.is_empty() {
            return out;
        }
        after = page.last().map(|x| x.id);
        out.extend(page);
    }
}

#[tokio::test]
async fn list_domains_pages_in_creation_order_with_status() {
    for_each_backend(|b| async move {
        assert!(b.dir.list_domains(None, 10).await.unwrap().is_empty());
        for n in ["a.test", "b.test", "c.test", "d.test", "e.test"] {
            b.dir.create_domain(n).await.unwrap();
        }
        let all = all_domains(&b.dir).await;
        let names: Vec<_> = all.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a.test", "b.test", "c.test", "d.test", "e.test"]);
        assert!(all.iter().all(|d| d.status == DomainStatus::Active && d.created_at > 0));
        // Exactly the page size is returned, no more.
        assert_eq!(b.dir.list_domains(None, 3).await.unwrap().len(), 3);
    })
    .await;
}

#[tokio::test]
async fn list_page_limits_are_validated() {
    for_each_backend(|b| async move {
        kind!(b.dir.list_domains(None, 0).await, Error::Invalid(_));
        kind!(b.dir.list_domains(None, MAX_LIST_PAGE + 1).await, Error::Invalid(_));
        let dom = b.dir.create_domain("lim.test").await.unwrap();
        kind!(b.dir.list_accounts(dom, None, 0).await, Error::Invalid(_));
        kind!(b.dir.list_aliases(dom, None, MAX_LIST_PAGE + 1).await, Error::Invalid(_));
        assert!(b.dir.list_domains(None, MAX_LIST_PAGE).await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn find_domain_normalizes_and_misses_cleanly() {
    for_each_backend(|b| async move {
        let id = b.dir.create_domain("bücher.example").await.unwrap();
        let found = b.dir.find_domain(" BÜCHER.example. ").await.unwrap().unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.name, "xn--bcher-kva.example");
        assert!(b.dir.find_domain("nope.example").await.unwrap().is_none());
        kind!(b.dir.find_domain("a b.example").await, Error::Invalid(_));
    })
    .await;
}

#[tokio::test]
async fn disabled_domain_stops_resolving_and_authenticating() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let auth = LocalAuthenticator::new(d.clone()).await.unwrap();
        let dom = d.create_domain("off.test").await.unwrap();
        let acc = d.create_account("alice", dom, Some("pw-1234567"), None).await.unwrap();
        d.add_alias("ally", dom, acc).await.unwrap();
        assert_eq!(auth.verify("alice@off.test", "pw-1234567").await.unwrap(), Some(acc));

        d.set_domain_status(dom, DomainStatus::Disabled).await.unwrap();
        assert_eq!(
            d.find_domain("off.test").await.unwrap().unwrap().status,
            DomainStatus::Disabled
        );
        kind!(d.resolve("alice@off.test").await, Error::NotFound(_));
        kind!(d.resolve("ally@off.test").await, Error::NotFound(_));
        assert_eq!(auth.verify("alice@off.test", "pw-1234567").await.unwrap(), None);

        // Another domain is untouched.
        let other = d.create_domain("on.test").await.unwrap();
        let o = d.create_account("bob", other, None, None).await.unwrap();
        assert_eq!(d.resolve("bob@on.test").await.unwrap(), Resolved::Account(o));

        d.set_domain_status(dom, DomainStatus::Active).await.unwrap();
        assert_eq!(d.resolve("alice@off.test").await.unwrap(), Resolved::Account(acc));
        assert_eq!(auth.verify("alice@off.test", "pw-1234567").await.unwrap(), Some(acc));
        kind!(d.set_domain_status(DomainId(424242), DomainStatus::Disabled).await, Error::NotFound(_));
    })
    .await;
}

#[tokio::test]
async fn find_account_is_exact_and_ignores_aliases_and_lists() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let dom = d.create_domain("fa.test").await.unwrap();
        let other = d.create_domain("fb.test").await.unwrap();
        let acc = d.create_account("alice", dom, Some("pw-1234567"), None).await.unwrap();
        let nopw = d.create_account("bob", dom, None, None).await.unwrap();
        d.add_alias("ally", dom, acc).await.unwrap();
        d.create_list("team", dom).await.unwrap();
        d.set_status(nopw, AccountStatus::Disabled).await.unwrap();

        let a = d.find_account("Alice@FA.test").await.unwrap().unwrap();
        assert_eq!((a.id, a.domain, a.local.as_str()), (acc, dom, "alice"));
        assert_eq!(a.status, AccountStatus::Active);
        assert!(a.has_password);
        let n = d.find_account("bob@fa.test").await.unwrap().unwrap();
        assert!(!n.has_password);
        assert_eq!(n.status, AccountStatus::Disabled);

        assert!(d.find_account("ally@fa.test").await.unwrap().is_none());
        assert!(d.find_account("team@fa.test").await.unwrap().is_none());
        assert!(d.find_account("alice@fb.test").await.unwrap().is_none());
        assert!(d.find_account("nobody@fa.test").await.unwrap().is_none());
        assert!(d.find_account("alice@nodomain.test").await.unwrap().is_none());
        kind!(d.find_account("no-at-sign").await, Error::Invalid(_));
        let _ = other;
    })
    .await;
}

#[tokio::test]
async fn list_accounts_is_per_domain_and_paged() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let a = d.create_domain("la.test").await.unwrap();
        let o = d.create_domain("lb.test").await.unwrap();
        for n in ["u1", "u2", "u3", "u4", "u5"] {
            d.create_account(n, a, Some("pw-1234567"), None).await.unwrap();
        }
        d.create_account("stranger", o, None, None).await.unwrap();

        let p1 = d.list_accounts(a, None, 2).await.unwrap();
        let p2 = d.list_accounts(a, p1.last().map(|x| x.id), 2).await.unwrap();
        let p3 = d.list_accounts(a, p2.last().map(|x| x.id), 2).await.unwrap();
        let p4 = d.list_accounts(a, p3.last().map(|x| x.id), 2).await.unwrap();
        let names: Vec<_> = p1.iter().chain(&p2).chain(&p3).map(|x| x.local.as_str()).collect();
        assert_eq!(names, ["u1", "u2", "u3", "u4", "u5"]);
        assert_eq!((p1.len(), p2.len(), p3.len(), p4.len()), (2, 2, 1, 0));
        assert!(p1.iter().all(|x| x.has_password && x.domain == a && x.status == AccountStatus::Active));
        // The other tenant sees only its own account.
        let theirs = d.list_accounts(o, None, 10).await.unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].local, "stranger");
        assert!(!theirs[0].has_password);
        assert!(d.list_accounts(DomainId(424242), None, 10).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn list_aliases_is_per_domain_and_names_the_target() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let a = d.create_domain("ala.test").await.unwrap();
        let o = d.create_domain("alb.test").await.unwrap();
        let alice = d.create_account("alice", a, None, None).await.unwrap();
        let eve = d.create_account("eve", o, None, None).await.unwrap();
        for n in ["x1", "x2", "x3"] {
            d.add_alias(n, a, alice).await.unwrap();
        }
        d.add_alias("x1", o, eve).await.unwrap();

        let p1 = d.list_aliases(a, None, 2).await.unwrap();
        let p2 = d.list_aliases(a, p1.last().map(|x| x.id), 2).await.unwrap();
        let names: Vec<_> = p1.iter().chain(&p2).map(|x| x.local.as_str()).collect();
        assert_eq!(names, ["x1", "x2", "x3"]);
        assert!(p1.iter().chain(&p2).all(|x| x.account == alice
            && x.account_local == "alice"
            && x.account_domain == "ala.test"));
        let theirs = d.list_aliases(o, None, 10).await.unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].account_domain, "alb.test");
    })
    .await;
}

#[tokio::test]
async fn remove_alias_deletes_only_that_alias() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
        let a = d.create_domain("ra.test").await.unwrap();
        let o = d.create_domain("rb.test").await.unwrap();
        let alice = d.create_account("alice", a, None, None).await.unwrap();
        let eve = d.create_account("eve", o, None, None).await.unwrap();
        d.add_alias("info", a, alice).await.unwrap();
        d.add_alias("info", o, eve).await.unwrap();

        d.remove_alias("INFO@ra.test").await.unwrap();
        kind!(d.resolve("info@ra.test").await, Error::NotFound(_));
        assert_eq!(d.resolve("info@rb.test").await.unwrap(), Resolved::Account(eve));
        kind!(d.remove_alias("info@ra.test").await, Error::NotFound(_));
        // An account is not an alias.
        kind!(d.remove_alias("alice@ra.test").await, Error::NotFound(_));
        assert_eq!(d.resolve("alice@ra.test").await.unwrap(), Resolved::Account(alice));
        kind!(d.remove_alias("info@missing.test").await, Error::NotFound(_));
        // The freed address can be used by an account again.
        d.create_account("info", a, None, None).await.unwrap();
        let _ = AccountId(0);
    })
    .await;
}
