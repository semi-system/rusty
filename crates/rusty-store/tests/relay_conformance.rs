//! `domain_relays` (admin web design, section 14.4): the relay a tenant chooses for its domain. The store checks what the tenant
//! entered (`rusty_core::relay_policy`), the table repeats the checks, a replaced relay forgets its last check, and the relay goes with the domain.
mod common;

use common::backends::*;
use rusty_core::config::RelayTls;
use rusty_core::relay_policy::{validate_tenant_relay, TenantRelay};
use rusty_core::{DomainId, Error};

async fn domain(b: &Backend, name: &str) -> DomainId {
    b.dir.create_domain(name).await.unwrap()
}

fn relay(host: &str, port: u16, tls: RelayTls, user: Option<&str>) -> TenantRelay {
    validate_tenant_relay(host, port, tls, user).unwrap()
}

#[tokio::test]
async fn a_relay_is_stored_and_read_back_normalised() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "relay.test").await;
        let s = b.relay();
        assert!(s.get(d).await.unwrap().is_none(), "{n}");
        let row = s.set_at(d, &relay("SMTP.Mail.NET.", 587, RelayTls::Starttls, Some("user")), Some("spf.mail.net"), 100).await.unwrap();
        assert_eq!((row.host.as_str(), row.port, row.tls.as_str(), row.username.as_deref(), row.spf_include.as_deref()), ("smtp.mail.net", 587, "starttls", Some("user"), Some("spf.mail.net")), "{n}");
        assert_eq!((row.created_at, row.updated_at, row.check_status.clone(), row.checked_at), (100, 100, None, None), "{n}");
        assert_eq!(s.get(d).await.unwrap().unwrap(), row, "{n}");
        // No login and no SPF include are fine.
        let d2 = domain(&b, "plain.test").await;
        let r2 = s.set_at(d2, &relay("relay.mail.net", 25, RelayTls::None, None), None, 5).await.unwrap();
        assert_eq!((r2.tls.as_str(), r2.username, r2.spf_include), ("none", None, None), "{n}");
    })
    .await;
}

#[tokio::test]
async fn setting_again_replaces_the_relay_keeps_its_age_and_forgets_the_check() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "again.test").await;
        let s = b.relay();
        s.set_at(d, &relay("one.mail.net", 587, RelayTls::Starttls, Some("a")), None, 100).await.unwrap();
        assert!(s.record_check_at(d, "ok", 150).await.unwrap(), "{n}");
        let r = s.get(d).await.unwrap().unwrap();
        assert_eq!((r.check_status.as_deref(), r.checked_at), (Some("ok"), Some(150)), "{n}");
        let r = s.set_at(d, &relay("two.mail.net", 465, RelayTls::Implicit, None), Some("spf.two.net"), 300).await.unwrap();
        assert_eq!((r.host.as_str(), r.port, r.username, r.created_at, r.updated_at, r.check_status, r.checked_at), ("two.mail.net", 465, None, 100, 300, None, None), "{n}: the check described another server");
    })
    .await;
}

#[tokio::test]
async fn checks_record_only_the_fixed_categories_and_only_for_a_domain_that_has_a_relay() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (d, none) = (domain(&b, "check.test").await, domain(&b, "norelay.test").await);
        let s = b.relay();
        s.set_at(d, &relay("r.mail.net", 587, RelayTls::Starttls, Some("u")), None, 10).await.unwrap();
        for status in rusty_store::CHECK_STATUSES {
            assert!(s.record_check_at(d, status, 20).await.unwrap(), "{n}: {status}");
            assert_eq!(s.get(d).await.unwrap().unwrap().check_status.as_deref(), Some(status), "{n}");
        }
        for bad in ["", "OK", "535 5.7.8 bad credentials for bob", "ok; DROP TABLE domain_relays"] {
            assert!(matches!(s.record_check_at(d, bad, 30).await, Err(Error::Invalid(_))), "{n}: {bad:?}");
        }
        assert!(!s.record_check_at(none, "ok", 30).await.unwrap(), "{n}: nothing to record on");
        assert!(s.get(none).await.unwrap().is_none(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn what_a_tenant_may_not_choose_is_refused_by_the_store_even_for_a_hand_built_relay() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "refuse.test").await;
        let s = b.relay();
        let mk = |host: &str, port, tls, user: Option<&str>| TenantRelay { host: host.into(), port, tls, username: user.map(str::to_owned) };
        let bad = [
            mk("smtp.mail.net", 22, RelayTls::Starttls, None),
            mk("localhost", 587, RelayTls::Starttls, None),
            mk("10.0.0.1", 587, RelayTls::Starttls, None),
            mk("169.254.169.254", 25, RelayTls::None, None),
            mk("smtp.mail.net", 587, RelayTls::None, Some("u")),
            mk("smtp.mail.net", 587, RelayTls::Implicit, None),
            mk("SMTP.MAIL.NET", 587, RelayTls::Starttls, Some("a\nb")),
            mk("a.internal", 587, RelayTls::Starttls, None),
        ];
        for r in bad {
            assert!(matches!(s.set_at(d, &r, None, 10).await, Err(Error::Invalid(_))), "{n}: {r:?}");
        }
        for spf in ["", "bad name", "a;b", "include:x"] {
            assert!(matches!(s.set_at(d, &relay("smtp.mail.net", 587, RelayTls::Starttls, None), Some(spf), 10).await, Err(Error::Invalid(_))), "{n}: {spf:?}");
        }
        assert!(s.get(d).await.unwrap().is_none(), "{n}: nothing was stored");
    })
    .await;
}

#[tokio::test]
async fn the_table_refuses_what_the_store_would_for_any_other_writer() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = domain(&b, "second.test").await;
        let ins = |cols: &str, vals: &str| format!("INSERT INTO domain_relays (domain_id, {cols}, created_at, updated_at) VALUES ({}, {vals}, 1, 1)", d.0);
        for (what, sql) in [
            ("port 22", ins("host, port, tls", "'a.mail.net', 22, 'starttls'")),
            ("tls off with a login", ins("host, port, tls, username", "'a.mail.net', 587, 'none', 'u'")),
            ("implicit on 587", ins("host, port, tls", "'a.mail.net', 587, 'implicit'")),
            ("upper-case host", ins("host, port, tls", "'A.MAIL.NET', 587, 'starttls'")),
            ("space in host", ins("host, port, tls", "'a b.net', 587, 'starttls'")),
            ("empty user", ins("host, port, tls, username", "'a.mail.net', 587, 'starttls', ''")),
            ("control in user", ins("host, port, tls, username", "'a.mail.net', 587, 'starttls', E'a\\nb'")),
            ("status without time", ins("host, port, tls, check_status", "'a.mail.net', 587, 'starttls', 'ok'")),
            ("free-text status", ins("host, port, tls, check_status, checked_at", "'a.mail.net', 587, 'starttls', '535 nope', 1")),
        ] {
            assert!(try_execute(&b, &sql, &[]).await.is_err(), "{n}: {what}");
        }
        assert!(b.relay().get(d).await.unwrap().is_none(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn a_relay_goes_with_its_domain_and_one_domain_never_sees_anothers() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (gone, kept) = (domain(&b, "gone.test").await, domain(&b, "kept.test").await);
        let s = b.relay();
        s.set_at(gone, &relay("g.mail.net", 587, RelayTls::Starttls, None), None, 10).await.unwrap();
        s.set_at(kept, &relay("k.mail.net", 465, RelayTls::Implicit, None), None, 10).await.unwrap();
        assert_eq!(s.get(kept).await.unwrap().unwrap().host, "k.mail.net", "{n}");
        run(&b, &format!("DELETE FROM domains WHERE id = {}", gone.0)).await;
        assert!(s.get(gone).await.unwrap().is_none(), "{n}: nothing outlives its domain");
        assert!(s.get(kept).await.unwrap().is_some(), "{n}");
        assert!(s.remove(kept).await.unwrap(), "{n}");
        assert!(!s.remove(kept).await.unwrap(), "{n}: removing twice is not an error");
        assert!(s.get(kept).await.unwrap().is_none(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn a_relay_for_a_domain_that_does_not_exist_is_an_error_not_a_row() {
    for_each_backend(|b| async move {
        let n = b.name;
        let r = b.relay().set_at(DomainId(987_654), &relay("smtp.mail.net", 587, RelayTls::Starttls, None), None, 10).await;
        assert!(r.is_err(), "{n}: {r:?}");
        assert!(b.relay().get(DomainId(987_654)).await.unwrap().is_none(), "{n}");
    })
    .await;
}
