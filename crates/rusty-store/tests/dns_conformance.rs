//! DnsRepo / DnsStore conformance (U10 part 1b): the persistence of DNS validation state, on both
//! backends, one body. Upsert, transitions, history cap, tenant isolation, cascade, token hygiene.
mod common;

use common::backends::*;
use rusty_core::dns_state::{AlertChange, Observation, RecordKey, RecordStatus};
use rusty_core::{DomainId, Error};
use rusty_store::repo::DnsConfig;
use rusty_store::{DnsStore, MAX_RECORDS_PER_DOMAIN};

const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn key(name: &str, purpose: &str) -> RecordKey {
    RecordKey { name: name.into(), rtype: "TXT".into(), purpose: purpose.into() }
}

fn obs(name: &str, status: RecordStatus, hash: &str) -> Observation {
    Observation { key: key(name, "test"), expected_hash: hash.into(), status, seen: Some(format!("seen {name}")) }
}

async fn domain(b: &Backend, name: &str) -> DomainId {
    b.dir.create_domain(name).await.unwrap()
}

fn cfg() -> DnsConfig {
    DnsConfig {
        dkim_selector: Some("rusty".into()),
        dkim_algorithm: Some("rsa".into()),
        dmarc_policy: "none".into(),
        dmarc_rua: Some("dmarc@example.test".into()),
        outbound_ref: Some("ip:203.0.113.5".into()),
    }
}

async fn count(b: &Backend, table: &str, d: DomainId) -> i64 {
    scalar(b, &format!("SELECT count(*) FROM {table} WHERE domain_id = {}", d.0)).await
}

#[tokio::test]
async fn config_upsert_roundtrip_and_validation() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let d = domain(&b, "cfg.test").await;
        assert_eq!(s.config(d).await.unwrap(), None, "{n}");
        s.put_config_at(d, &cfg(), 100).await.unwrap();
        let row = s.config(d).await.unwrap().unwrap();
        assert_eq!(row.config, cfg(), "{n}");
        assert!(!row.has_ownership_token && row.expected_hash.is_none(), "{n}");
        assert_eq!((row.created_at, row.updated_at), (100, 100), "{n}");

        // token and expected hash survive a settings change; created_at stays, updated_at moves
        let _ = s.new_ownership_token(d).await.unwrap();
        s.acknowledge(d, H1).await.unwrap();
        let mut c2 = cfg();
        c2.dmarc_policy = "quarantine".into();
        c2.dkim_selector = Some("s2".into());
        s.put_config_at(d, &c2, 200).await.unwrap();
        let row = s.config(d).await.unwrap().unwrap();
        assert_eq!(row.config, c2, "{n}");
        assert!(row.has_ownership_token, "{n}");
        assert_eq!(row.expected_hash.as_deref(), Some(H1), "{n}");
        assert_eq!((row.created_at, row.updated_at), (100, 200), "{n}");

        // validation happens before the database
        for bad in [
            DnsConfig { dkim_selector: Some("-x".into()), ..cfg() },
            DnsConfig { dkim_selector: Some("a b".into()), ..cfg() },
            DnsConfig { dkim_algorithm: Some("dsa".into()), ..cfg() },
            DnsConfig { dkim_selector: None, ..cfg() },
            DnsConfig { dmarc_policy: "yes".into(), ..cfg() },
            DnsConfig { dmarc_rua: Some("no-at-sign".into()), ..cfg() },
            DnsConfig { dmarc_rua: Some("a@b.test;evil".into()), ..cfg() },
            DnsConfig { outbound_ref: Some("x\ny".into()), ..cfg() },
        ] {
            assert!(matches!(s.put_config(d, &bad).await, Err(Error::Invalid(_))), "{n} {bad:?}");
        }
        assert_eq!(s.config(d).await.unwrap().unwrap().config, c2, "{n}: a refused write changes nothing");

        // a domain that does not exist
        assert!(matches!(s.put_config(DomainId(9_999_999), &cfg()).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(s.acknowledge(DomainId(9_999_999), H1).await, Err(Error::NotFound(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn records_changed_compares_with_the_applied_hash() {
    for_each_backend(|b| async move {
        let s = b.dns();
        let d = domain(&b, "chg.test").await;
        assert!(!s.records_changed(d, H1).await.unwrap(), "{}: nothing applied yet", b.name);
        s.acknowledge(d, H1).await.unwrap();
        assert!(!s.records_changed(d, H1).await.unwrap());
        assert!(s.records_changed(d, H2).await.unwrap(), "{}: a DKIM or IP change changes the set hash", b.name);
        assert!(matches!(s.acknowledge(d, "short").await, Err(Error::Invalid(_))));
        s.acknowledge(d, H2).await.unwrap();
        assert!(!s.records_changed(d, H2).await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn ownership_token_is_random_hashed_and_compared_by_value() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let (d1, d2) = (domain(&b, "own1.test").await, domain(&b, "own2.test").await);
        assert!(!s.verify_ownership(d1, "anything").await.unwrap(), "{n}: no token yet");
        let t1 = s.new_ownership_token(d1).await.unwrap();
        let t2 = s.new_ownership_token(d2).await.unwrap();
        assert_eq!(t1.expose().len(), 64, "{n}");
        assert_ne!(t1.expose(), t2.expose(), "{n}");
        assert_eq!(format!("{t1:?}"), "OwnershipToken(<redacted>)");
        assert!(s.verify_ownership(d1, t1.expose()).await.unwrap(), "{n}");
        assert!(s.verify_ownership(d1, &t1.expose().to_uppercase()).await.unwrap(), "{n}: hex is case-insensitive");
        assert!(!s.verify_ownership(d1, t2.expose()).await.unwrap(), "{n}: another tenant's token");
        assert!(!s.verify_ownership(d1, "").await.unwrap() && !s.verify_ownership(d1, &t1.expose()[..63]).await.unwrap(), "{n}");
        let v = s.ownership_verifier(d1).await.unwrap().unwrap();
        assert_eq!(format!("{v:?}"), "TokenVerifier(<redacted>)");
        assert!(v.accepts(t1.expose()) && !v.accepts("x"), "{n}");

        // only a hash is stored, never the token
        let stored = fetch_strings(&b, &format!("SELECT ownership_token_hash FROM domain_dns_config WHERE domain_id = {}", d1.0), &[]).await;
        assert_eq!(stored.len(), 1, "{n}");
        assert_eq!(stored[0].len(), 64, "{n}");
        assert_ne!(stored[0], t1.expose(), "{n}: the token itself must not be stored");
        let all: Vec<String> = fetch_strings(&b, "SELECT ownership_token_hash FROM domain_dns_config", &[]).await;
        assert!(all.iter().all(|h| h != t1.expose() && h != t2.expose()), "{n}");

        // a new token replaces the old one
        let t1b = s.new_ownership_token(d1).await.unwrap();
        assert!(!s.verify_ownership(d1, t1.expose()).await.unwrap(), "{n}: the old token is dead");
        assert!(s.verify_ownership(d1, t1b.expose()).await.unwrap(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn check_records_state_and_exactly_one_event_per_transition() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let d = domain(&b, "drift.test").await;
        let set = |st: RecordStatus| vec![obs("dkim.drift.test.", st, H1), obs("mx.drift.test.", RecordStatus::Ok, H1)];

        let steps = s.record_check_at(d, &set(RecordStatus::Ok), 2, 1000).await.unwrap();
        assert_eq!(steps.len(), 2, "{n}");
        let st = s.states(d).await.unwrap();
        assert_eq!(st.len(), 2, "{n}");
        assert!(st.iter().all(|r| r.status == RecordStatus::Ok && r.first_ok_at == Some(1000) && r.consecutive_failures == 0), "{n}: {st:?}");
        assert_eq!(st[0].key.name, "dkim.drift.test.", "{n}: ordered by key");
        assert_eq!(st[0].seen.as_deref(), Some("seen dkim.drift.test."), "{n}");

        // the same status again: no new event, last_checked moves, status_since does not
        s.record_check_at(d, &set(RecordStatus::Ok), 2, 2000).await.unwrap();
        let st = s.states(d).await.unwrap();
        assert_eq!((st[0].last_checked_at, st[0].status_since, st[0].last_ok_at), (2000, 1000, Some(2000)), "{n}");
        assert_eq!(s.events(d, 0, 50).await.unwrap().len(), 2, "{n}: two first observations, nothing more");

        // the DKIM record is deleted at the registrar
        let a = s.record_check_at(d, &set(RecordStatus::Missing), 2, 3000).await.unwrap();
        assert_eq!(a[0].alert, AlertChange::None, "{n}: first failure");
        let a = s.record_check_at(d, &set(RecordStatus::Missing), 2, 4000).await.unwrap();
        assert_eq!(a[0].alert, AlertChange::Raised, "{n}: second failure raises");
        let a = s.record_check_at(d, &set(RecordStatus::Missing), 2, 5000).await.unwrap();
        assert_eq!(a[0].alert, AlertChange::None, "{n}: raised once");
        let ev = s.events(d, 2500, 50).await.unwrap();
        assert_eq!(ev.len(), 1, "{n}: exactly one ok -> missing event: {ev:?}");
        assert_eq!((ev[0].from, ev[0].to, ev[0].at), (Some(RecordStatus::Ok), RecordStatus::Missing, 3000), "{n}");
        assert_eq!(ev[0].key.name, "dkim.drift.test.", "{n}");
        let st = s.states(d).await.unwrap();
        assert_eq!((st[0].consecutive_failures, st[0].status_since, st[0].first_ok_at, st[0].last_ok_at), (3, 3000, Some(1000), Some(2000)), "{n}");

        // restored
        let a = s.record_check_at(d, &set(RecordStatus::Ok), 2, 6000).await.unwrap();
        assert_eq!(a[0].alert, AlertChange::Cleared, "{n}");
        let ev = s.events(d, 0, 50).await.unwrap();
        assert_eq!(ev.len(), 4, "{n}: 2 first observations + ok->missing + missing->ok");
        assert_eq!((ev[0].from, ev[0].to), (Some(RecordStatus::Missing), RecordStatus::Ok), "{n}: newest first");
        assert_eq!(s.events(d, 5500, 50).await.unwrap().len(), 1, "{n}: --since");
        assert_eq!(s.events(d, 0, 2).await.unwrap().len(), 2, "{n}: limit");
    })
    .await;
}

#[tokio::test]
async fn a_changed_expectation_starts_over_and_removed_records_are_forgotten() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let d = domain(&b, "rot.test").await;
        s.record_check_at(d, &[obs("dkim.rot.test.", RecordStatus::Ok, H1), obs("old.rot.test.", RecordStatus::Ok, H1)], 2, 100).await.unwrap();
        // key rotation: new expected hash, not published yet
        let st = s.record_check_at(d, &[obs("dkim.rot.test.", RecordStatus::Mismatch, H2)], 2, 200).await.unwrap();
        assert!(st[0].expectation_changed, "{n}");
        let rows = s.states(d).await.unwrap();
        assert_eq!(rows.len(), 1, "{n}: the record that left the expected set is gone");
        assert_eq!((rows[0].expected_hash.as_str(), rows[0].first_ok_at, rows[0].consecutive_failures), (H2, None, 1), "{n}");
        let a = s.record_check_at(d, &[obs("dkim.rot.test.", RecordStatus::Mismatch, H2)], 2, 300).await.unwrap();
        assert_eq!(a[0].alert, AlertChange::None, "{n}: a key that was never published is not a deleted record");
    })
    .await;
}

#[tokio::test]
async fn history_is_capped_per_domain_and_keeps_the_newest() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = DnsStore::with_event_cap(b.nrepo.clone(), 5);
        let (d1, d2) = (domain(&b, "cap1.test").await, domain(&b, "cap2.test").await);
        s.record_check_at(d2, &[obs("r.cap2.test.", RecordStatus::Ok, H1)], 3, 10).await.unwrap();
        for i in 0..12i64 {
            let st = if i % 2 == 0 { RecordStatus::Ok } else { RecordStatus::Missing };
            s.record_check_at(d1, &[obs("r.cap1.test.", st, H1)], 3, 100 + i).await.unwrap();
        }
        let ev = s.events(d1, 0, 50).await.unwrap();
        assert_eq!(ev.len(), 5, "{n}: capped");
        assert_eq!(ev.iter().map(|e| e.at).collect::<Vec<_>>(), vec![111, 110, 109, 108, 107], "{n}: the newest five");
        assert_eq!(s.events(d2, 0, 50).await.unwrap().len(), 1, "{n}: another domain's history is not trimmed");
        // a cap that is not reached trims nothing
        assert_eq!(s.events(d1, 109, 50).await.unwrap().len(), 3, "{n}");
    })
    .await;
}

#[tokio::test]
async fn tenants_do_not_see_each_other() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let (a, c) = (domain(&b, "ten-a.test").await, domain(&b, "ten-c.test").await);
        // the same record keys in both domains
        let same = |st| vec![obs("x._domainkey.shared.test.", st, H1)];
        s.record_check_at(a, &same(RecordStatus::Ok), 1, 100).await.unwrap();
        s.record_check_at(c, &same(RecordStatus::Ok), 1, 100).await.unwrap();
        s.put_config(a, &cfg()).await.unwrap();
        let ta = s.new_ownership_token(a).await.unwrap();
        s.acknowledge(c, H2).await.unwrap();

        s.record_check_at(a, &same(RecordStatus::Missing), 1, 200).await.unwrap();
        assert_eq!(s.states(c).await.unwrap()[0].status, RecordStatus::Ok, "{n}: a check of A never touches C");
        assert_eq!(s.events(c, 0, 50).await.unwrap().len(), 1, "{n}");
        assert_eq!(s.events(a, 0, 50).await.unwrap().len(), 2, "{n}");
        assert!(s.config(c).await.unwrap().unwrap().config.dkim_selector.is_none(), "{n}: A's settings are not C's");
        assert!(!s.verify_ownership(c, ta.expose()).await.unwrap(), "{n}: A's token proves nothing for C");
        assert!(!s.records_changed(a, H1).await.unwrap(), "{n}");
        assert!(s.records_changed(c, H1).await.unwrap(), "{n}: C's applied hash is C's own");
        // a missing domain is refused, not silently written to another tenant
        assert!(matches!(s.record_check(DomainId(9_999_999), &same(RecordStatus::Ok), 1).await, Err(Error::NotFound(_))), "{n}");
        assert_eq!(count(&b, "domain_dns_state", DomainId(9_999_999)).await, 0, "{n}");
    })
    .await;
}

#[tokio::test]
async fn rows_cascade_with_their_domain() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let (gone, kept) = (domain(&b, "gone.test").await, domain(&b, "kept.test").await);
        for d in [gone, kept] {
            s.put_config(d, &cfg()).await.unwrap();
            s.new_ownership_token(d).await.unwrap();
            s.record_check_at(d, &[obs("r.test.", RecordStatus::Ok, H1), obs("q.test.", RecordStatus::Missing, H1)], 1, 100).await.unwrap();
        }
        for t in ["domain_dns_config", "domain_dns_state", "domain_dns_event"] {
            assert!(count(&b, t, gone).await > 0 && count(&b, t, kept).await > 0, "{n} {t}");
        }
        run(&b, &format!("DELETE FROM domains WHERE id = {}", gone.0)).await;
        for t in ["domain_dns_config", "domain_dns_state", "domain_dns_event"] {
            assert_eq!(count(&b, t, gone).await, 0, "{n} {t}: cascade");
            assert!(count(&b, t, kept).await > 0, "{n} {t}: the other tenant keeps its rows");
        }
    })
    .await;
}

#[tokio::test]
async fn bad_observations_are_refused_before_anything_is_written() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let d = domain(&b, "bad.test").await;
        let good = obs("ok.bad.test.", RecordStatus::Ok, H1);
        let dup = vec![good.clone(), good.clone()];
        let mut bad_hash = good.clone();
        bad_hash.expected_hash = "zz".into();
        let mut bad_type = good.clone();
        bad_type.key.rtype = "SOA".into();
        let mut empty_name = good.clone();
        empty_name.key.name = String::new();
        let too_many: Vec<Observation> =
            (0..=MAX_RECORDS_PER_DOMAIN).map(|i| obs(&format!("r{i}.bad.test."), RecordStatus::Ok, H1)).collect();
        for set in [dup, vec![good.clone(), bad_hash], vec![bad_type], vec![empty_name], too_many] {
            assert!(matches!(s.record_check(d, &set, 1).await, Err(Error::Invalid(_))), "{n}");
        }
        assert!(s.states(d).await.unwrap().is_empty() && s.events(d, 0, 10).await.unwrap().is_empty(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn evidence_is_cut_and_cleaned_before_it_is_stored() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let d = domain(&b, "evid.test").await;
        let mut o = obs("e.evid.test.", RecordStatus::Mismatch, H1);
        o.seen = Some(format!("v=spf1\u{0}\n{}", "é".repeat(600)));
        s.record_check_at(d, &[o], 3, 100).await.unwrap();
        let seen = s.states(d).await.unwrap()[0].seen.clone().unwrap();
        assert!(seen.len() <= 512 && seen.starts_with("v=spf1é"), "{n}: {}", seen.len());
    })
    .await;
}

#[tokio::test]
async fn concurrent_checks_of_one_domain_record_a_change_once() {
    for_each_backend(|b| async move {
        let n = b.name;
        let s = b.dns();
        let d = domain(&b, "race.test").await;
        s.record_check_at(d, &[obs("r.race.test.", RecordStatus::Ok, H1)], 3, 100).await.unwrap();
        let miss = [obs("r.race.test.", RecordStatus::Missing, H1)];
        let (a, c) = tokio::join!(s.record_check_at(d, &miss, 3, 200), s.record_check_at(d, &miss, 3, 201));
        a.unwrap();
        c.unwrap();
        let ev = s.events(d, 150, 50).await.unwrap();
        assert_eq!(ev.len(), 1, "{n}: serialised on the domain: one ok -> missing event, not two: {ev:?}");
        assert_eq!(s.states(d).await.unwrap()[0].consecutive_failures, 2, "{n}");
    })
    .await;
}
