//! The DNS page: generated records joined with the stored check state, the queued check, and the scope rules.
mod common;

use common::*;
use rusty_core::dns::{records_with_facts, MailFacts};
use rusty_core::dns_check::record_hash;
use rusty_core::dns_state::{Observation, RecordKey, RecordStatus};
use serde_json::json;

fn facts(ips: &[std::net::IpAddr]) -> MailFacts<'_> {
    MailFacts { mail_host: "mail.example.org", public_ips: ips, spf_include: None }
}

#[tokio::test]
async fn the_page_lists_the_records_with_their_last_check_and_a_new_key_adds_its_record() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let id = r.dir.find_domain("one.test").await.unwrap().unwrap().id;

    let page = c.get("/api/v1/domains/one.test/dns").await;
    assert_eq!(page.status, 200, "{}", page.body);
    conforms(&page.json(), "DnsRecords");
    let recs = page.json()["records"].as_array().unwrap().clone();
    let types: Vec<&str> = recs.iter().map(|x| x["type"].as_str().unwrap()).collect();
    for t in ["MX", "A", "TXT"] {
        assert!(types.contains(&t), "{t} in {types:?}");
    }
    assert!(recs.iter().all(|x| x["status"] == "unchecked" && x["last_checked_at"].is_null()), "nothing was checked yet");
    assert!(page.json()["warnings"].as_array().unwrap().is_empty(), "the rig has public addresses");
    let spf = recs.iter().find(|x| x["purpose"].as_str().is_some_and(|p| p.starts_with("SPF"))).expect("an SPF record");
    assert!(spf["value"].as_str().unwrap().contains("203.0.114.5"));
    assert!(recs.iter().any(|x| x["name"].as_str().unwrap().starts_with('<')), "the placeholder that says how to make a DKIM key");

    // A check recorded the MX as ok: the page shows it, with its time.
    let ips: Vec<std::net::IpAddr> = vec!["203.0.114.5".parse().unwrap()];
    let (expected, _) = records_with_facts("one.test", &facts(&ips), &[]).unwrap();
    let mx = expected.iter().find(|x| x.rtype.as_str() == "MX").unwrap();
    let key = RecordKey { name: mx.name.to_ascii_lowercase(), rtype: "MX".into(), purpose: mx.purpose.to_owned() };
    let obs = Observation { key: key.clone(), expected_hash: record_hash(mx), status: RecordStatus::Ok, seen: None };
    rusty_store::DnsStore::with_repo(r.store.repos().dns.clone()).record_check(id, &[obs], 3).await.unwrap();
    let after = c.get("/api/v1/domains/one.test/dns").await.json();
    let row = after["records"].as_array().unwrap().iter().find(|x| x["type"] == "MX").unwrap();
    assert_eq!(row["status"], "ok");
    assert!(row["last_checked_at"].is_i64());
    assert!(after["records"].as_array().unwrap().iter().filter(|x| x["type"] != "MX").all(|x| x["status"] == "unchecked"));

    // A state recorded for a record that has since changed does not describe it: unchecked.
    let stale = Observation { key, expected_hash: "0".repeat(64), status: RecordStatus::Ok, seen: None };
    rusty_store::DnsStore::with_repo(r.store.repos().dns.clone()).record_check(id, &[stale], 3).await.unwrap();
    let after = c.get("/api/v1/domains/one.test/dns").await.json();
    assert_eq!(after["records"].as_array().unwrap().iter().find(|x| x["type"] == "MX").unwrap()["status"], "unchecked");

    // A DKIM key made in the web adds its record in place of the placeholder.
    c.post("/api/v1/domains/one.test/dkim", json!({})).await;
    let with_key = c.get("/api/v1/domains/one.test/dns").await.json();
    let dkim = with_key["records"].as_array().unwrap().iter().find(|x| x["name"].as_str().unwrap().contains("_domainkey")).expect("the DKIM record");
    assert!(dkim["value"].as_str().unwrap().starts_with("v=DKIM1; k=ed25519; p="));
    assert!(!with_key["records"].as_array().unwrap().iter().any(|x| x["name"].as_str().unwrap().starts_with('<')));
}

#[tokio::test]
async fn a_check_is_queued_for_the_service_that_queries_dns() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let q = c.post_empty("/api/v1/domains/one.test/dns/check").await;
    assert_eq!(q.status, 200, "{}", q.body);
    conforms(&q.json(), "Queued");
    assert_eq!(r.count_jobs("dns.check").await, 1);
    // Without the guards nothing is queued.
    let before = r.count_jobs("dns.check").await;
    for (what, h) in [
        ("no token", vec![("Cookie", c.cookie.as_str()), ("Origin", ORIGIN), ("Content-Type", "application/json")]),
        ("no origin", vec![("Cookie", c.cookie.as_str()), ("Content-Type", "application/json"), ("X-CSRF-Token", c.csrf.as_str())]),
    ] {
        assert_eq!(send(r.addr, "POST", "/api/v1/domains/one.test/dns/check", &h, "").await.status, 403, "{what}");
    }
    assert_eq!(send(r.addr, "POST", "/api/v1/domains/one.test/dns/check", &[("Origin", ORIGIN), ("Content-Type", "application/json")], "").await.status, 401);
    assert_eq!(r.count_jobs("dns.check").await, before);
}

#[tokio::test]
async fn another_tenants_dns_is_not_found_exactly_like_a_missing_domain() {
    let r = rig().await;
    make_admin(&r, "boss@one.test", true).await;
    make_admin(&r, "boss@two.test", true).await;
    let c = Client::sign_in(r.addr, "boss@one.test").await;
    for (m, foreign, missing) in [
        ("GET", "/api/v1/domains/two.test/dns", "/api/v1/domains/nowhere.test/dns"),
        ("POST", "/api/v1/domains/two.test/dns/check", "/api/v1/domains/nowhere.test/dns/check"),
    ] {
        let a = c.mutate(m, foreign, None).await;
        let z = c.mutate(m, missing, None).await;
        assert_eq!((a.status, a.body.as_str()), (404, "{\"error\":\"not_found\"}"), "{m} {foreign}");
        assert_eq!((a.status, &a.body), (z.status, &z.body), "{m}: must not reveal that the tenant exists");
    }
    assert_eq!(r.count_jobs("dns.check").await, 0, "nothing was queued for the other tenant");
    assert_eq!(c.get("/api/v1/domains/one.test/dns").await.status, 200);
    assert_eq!(send(r.addr, "GET", "/api/v1/domains/one.test/dns", &[], "").await.status, 401);
}

#[tokio::test]
async fn hosting_is_switched_by_the_platform_and_the_tenant_edits_its_own_records_within_the_rules() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    make_admin(&r, "boss@one.test", true).await;
    make_admin(&r, "boss@two.test", true).await;
    let root = Client::sign_in(r.addr, "root@home.test").await;
    let tenant = Client::sign_in(r.addr, "boss@one.test").await;
    let other = Client::sign_in(r.addr, "boss@two.test").await;

    // Every domain starts as it is today, and the tenant sees the names to set once it is hosted.
    let s = tenant.get("/api/v1/domains/one.test/dns/hosting").await;
    assert_eq!(s.status, 200, "{}", s.body);
    conforms(&s.json(), "DnsHosting");
    assert_eq!((s.json()["mode"].clone(), s.json()["name_servers"].clone()), (json!("external"), json!(["ns1.platform.example", "ns2.platform.example"])));
    // Only the platform hosts a domain.
    assert_eq!(tenant.mutate("PUT", "/api/v1/domains/one.test/dns/hosting", Some(json!({ "mode": "hosted" }))).await.status, 403);
    let on = root.mutate("PUT", "/api/v1/domains/one.test/dns/hosting", Some(json!({ "mode": "hosted" }))).await;
    assert_eq!((on.status, on.json()["mode"].clone()), (200, json!("hosted")), "{}", on.body);
    assert_eq!(root.mutate("PUT", "/api/v1/domains/one.test/dns/hosting", Some(json!({ "mode": "sideways" }))).await.status, 400);

    // The tenant adds records; a bad one is refused, and a CNAME stands alone at its name.
    let add = |name: &str, t: &str, v: &str| json!({ "name": name, "type": t, "value": v });
    let a = tenant.mutate("POST", "/api/v1/domains/one.test/dns/extra", Some(add("blog", "A", "192.0.2.80"))).await;
    assert_eq!(a.status, 200, "{}", a.body);
    conforms(&a.json(), "DnsHosting");
    assert_eq!(a.json()["extra"][0]["name"], json!("blog"));
    let id = a.json()["extra"][0]["id"].as_i64().unwrap();
    for bad in [add("*", "A", "192.0.2.1"), add("x", "A", "10"), add("@", "CNAME", "other.example.org"), add("x", "NS", "ns.example.org"), json!({ "name": "x", "type": "A", "value": "192.0.2.1", "ttl": 5 })] {
        assert_eq!(tenant.mutate("POST", "/api/v1/domains/one.test/dns/extra", Some(bad.clone())).await.status, 400, "{bad}");
    }
    assert_eq!(tenant.mutate("POST", "/api/v1/domains/one.test/dns/extra", Some(add("blog", "CNAME", "other.example.org"))).await.status, 409, "a CNAME next to an A");
    assert_eq!(tenant.mutate("POST", "/api/v1/domains/one.test/dns/extra", Some(add("www", "CNAME", "other.example.org"))).await.status, 200);
    assert_eq!(tenant.mutate("POST", "/api/v1/domains/one.test/dns/extra", Some(add("www", "A", "192.0.2.5"))).await.status, 409, "an A next to a CNAME");

    // Another tenant sees nothing of it and changes nothing; removing is for the owner.
    for (m, p, b) in [("GET", "/api/v1/domains/one.test/dns/hosting", None), ("POST", "/api/v1/domains/one.test/dns/extra", Some(add("evil", "A", "192.0.2.9"))), ("DELETE", "/api/v1/domains/one.test/dns/extra/1", None)] {
        assert_eq!(other.mutate(m, p, b).await.status, 404, "{m} {p}");
    }
    assert_eq!(tenant.mutate("DELETE", &format!("/api/v1/domains/one.test/dns/extra/{id}"), None).await.status, 200);
    assert_eq!(tenant.mutate("DELETE", &format!("/api/v1/domains/one.test/dns/extra/{id}"), None).await.status, 404);
    assert_eq!(tenant.get("/api/v1/domains/one.test/dns/hosting").await.json()["extra"].as_array().unwrap().len(), 1);
}
