//! The DKIM and relay endpoints: secrets are sealed and never returned, the relay rules are the SSRF policy of `rustyctl`, a domain administrator is confined to its
//! domain, and the responses have their contract shapes.
mod common;

use common::*;
use rusty_seal::Context;
use rusty_smtp::dkim::sealed_purpose;
use rusty_smtp::outbound::domain_route::RELAY_PASSWORD_PURPOSE;
use serde_json::json;

async fn domain_id(r: &Rig, name: &str) -> i64 {
    r.dir.find_domain(name).await.unwrap().unwrap().id.0
}

// ------------------------------------------------------------------------------------------------ DKIM

#[tokio::test]
async fn a_dkim_key_is_made_sealed_and_only_its_public_record_is_returned() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let id = domain_id(&r, "one.test").await;

    let made = c.post("/api/v1/domains/one.test/dkim", json!({})).await;
    assert_eq!(made.status, 201, "{}", made.body);
    conforms(&made.json(), "Dkim");
    let k = made.json();
    assert_eq!((k["selector"].as_str(), k["algorithm"].as_str(), k["sealed"].as_bool()), (Some("rusty-ed25519"), Some("ed25519"), Some(true)));
    assert_eq!(k["record_name"], "rusty-ed25519._domainkey.one.test");
    assert!(k["record_value"].as_str().unwrap().starts_with("v=DKIM1; k=ed25519; p="));
    assert!(!made.body.contains("PRIVATE") && !made.body.to_lowercase().contains("secret"), "no private key in the response");

    // The private half is a sealed secret that only the outbound service's key opens, bound to this domain.
    let row = r.store.repos().sealed.get("domain", id, &sealed_purpose("rusty-ed25519")).await.unwrap().expect("sealed");
    let pem = r.unsealer.unseal(&Context::new("domain", id, &sealed_purpose("rusty-ed25519")).unwrap(), &row.sealed).unwrap();
    assert!(String::from_utf8_lossy(&pem).contains("PRIVATE KEY"));
    assert!(!String::from_utf8_lossy(&row.sealed).contains("PRIVATE"), "the stored blob is ciphertext");
    assert!(r.unsealer.unseal(&Context::new("domain", id + 1, &sealed_purpose("rusty-ed25519")).unwrap(), &row.sealed).is_err(), "bound to its domain");

    // Listing shows the record; a published selector is never replaced; another selector is a new key.
    let list = c.get("/api/v1/domains/one.test/dkim").await;
    conforms(&list.json(), "DkimList");
    assert_eq!(list.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(c.post("/api/v1/domains/one.test/dkim", json!({})).await.status, 409);
    let second = c.post("/api/v1/domains/one.test/dkim", json!({ "selector": "s2" })).await;
    assert_eq!((second.status, second.json()["selector"].clone()), (201, "s2-ed25519".into()));
    assert_ne!(second.json()["record_value"], k["record_value"], "a different key");
    for bad in ["bad selector!", "-x", "a..b", "x".repeat(64).as_str(), ""] {
        assert_eq!(c.post("/api/v1/domains/one.test/dkim", json!({ "selector": bad })).await.status, 400, "{bad:?}");
    }
    assert_eq!(c.post("/api/v1/domains/one.test/dkim", json!({ "selector": "s3", "private_key": "x" })).await.status, 400, "the web never accepts a pasted key");
}

#[tokio::test]
async fn without_a_sealing_key_no_dkim_key_is_made_and_nothing_is_stored() {
    let r = rig_without_sealing().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let id = domain_id(&r, "one.test").await;
    let e = c.post("/api/v1/domains/one.test/dkim", json!({})).await;
    assert_eq!((e.status, e.json()["error"].clone()), (503, "unavailable".into()));
    assert_eq!(c.get("/api/v1/domains/one.test/dkim").await.json()["items"].as_array().unwrap().len(), 0);
    assert!(r.store.repos().sealed.meta("domain", id).await.unwrap().is_empty());
}

// ------------------------------------------------------------------------------------------------ relay

fn relay_body(host: &str, user: Option<&str>, password: Option<&str>) -> serde_json::Value {
    let mut b = json!({ "host": host, "port": 587, "tls": "starttls", "spf_include": "spf.mail.net" });
    if let Some(u) = user {
        b["username"] = u.into();
    }
    if let Some(p) = password {
        b["password"] = p.into();
    }
    b
}

#[tokio::test]
async fn a_relay_is_set_with_a_sealed_password_that_is_never_returned() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let id = domain_id(&r, "one.test").await;
    let empty = c.get("/api/v1/domains/one.test/relay").await;
    conforms(&empty.json(), "RelayState");
    assert!(empty.json()["relay"].is_null());

    let put = c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("SMTP.Mail.NET", Some("mailer"), Some("relay-secret-1")))).await;
    assert_eq!(put.status, 200, "{}", put.body);
    conforms(&put.json(), "RelayState");
    let rel = &put.json()["relay"];
    assert_eq!((rel["host"].as_str(), rel["port"].as_i64(), rel["tls"].as_str(), rel["username"].as_str(), rel["password_set"].as_bool()), (Some("smtp.mail.net"), Some(587), Some("starttls"), Some("mailer"), Some(true)));
    assert!(rel["check_status"].is_null() && rel["checked_at"].is_null());
    assert!(!put.body.contains("relay-secret-1"));
    let got = c.get("/api/v1/domains/one.test/relay").await;
    conforms(&got.json(), "RelayState");
    assert!(!got.body.contains("relay-secret-1") && got.json()["relay"]["password_set"] == true);

    // The password is in the sealed store and only the outbound service's key opens it, bound to this domain and purpose.
    let sealed = r.store.repos().sealed.get("domain", id, RELAY_PASSWORD_PURPOSE).await.unwrap().unwrap();
    let ctx = Context::new("domain", id, RELAY_PASSWORD_PURPOSE).unwrap();
    assert_eq!(&**r.unsealer.unseal(&ctx, &sealed.sealed).unwrap(), b"relay-secret-1");

    // A relay without a login drops the old password; a login needs its password again.
    let nologin = c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("two.mail.net", None, None))).await;
    assert_eq!(nologin.status, 200, "{}", nologin.body);
    assert_eq!(nologin.json()["relay"]["password_set"], false);
    assert!(r.store.repos().sealed.get("domain", id, RELAY_PASSWORD_PURPOSE).await.unwrap().is_none());
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("three.mail.net", Some("u"), None))).await.status, 400);

    // Check: queued for the outbound service; the job exists.
    let q = c.post_empty("/api/v1/domains/one.test/relay/check").await;
    assert_eq!(q.status, 200, "{}", q.body);
    conforms(&q.json(), "Queued");
    assert_eq!(r.count_jobs("relay.check").await, 1);

    // Delete: gone, with its password; twice is 404.
    c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("smtp.mail.net", Some("mailer"), Some("relay-secret-2")))).await;
    let del = c.mutate("DELETE", "/api/v1/domains/one.test/relay", None).await;
    assert_eq!(del.status, 200);
    conforms(&del.json(), "Empty");
    assert!(c.get("/api/v1/domains/one.test/relay").await.json()["relay"].is_null());
    assert!(r.store.repos().sealed.get("domain", id, RELAY_PASSWORD_PURPOSE).await.unwrap().is_none());
    assert_eq!(c.mutate("DELETE", "/api/v1/domains/one.test/relay", None).await.status, 404);
    assert_eq!(c.post_empty("/api/v1/domains/one.test/relay/check").await.status, 404, "nothing to check");
}

#[tokio::test]
async fn what_a_tenant_may_not_choose_is_refused_and_nothing_is_stored() {
    let r = rig().await;
    make_admin(&r, "boss@one.test", true).await;
    let c = Client::sign_in(r.addr, "boss@one.test").await;
    let id = domain_id(&r, "one.test").await;
    let put = |b: serde_json::Value| c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(b));
    let ok = relay_body("smtp.mail.net", None, None);
    for (what, mut b) in [
        ("inside host", relay_body("10.0.0.5", None, None)),
        ("metadata address", relay_body("169.254.169.254", None, None)),
        ("localhost", relay_body("localhost", None, None)),
        ("internal name", relay_body("mail.internal", None, None)),
        ("bad port", ok.clone()),
        ("bad tls", ok.clone()),
        ("login over clear", relay_body("smtp.mail.net", Some("u"), Some("pw-12345"))),
        ("password without login", relay_body("smtp.mail.net", None, Some("pw-12345"))),
        ("control character in the password", relay_body("smtp.mail.net", Some("u"), Some("pw\u{7}1"))),
        ("bad SPF include", ok.clone()),
        ("unknown field", ok.clone()),
    ] {
        match what {
            "bad port" => b["port"] = 22.into(),
            "bad tls" => b["tls"] = "ssl".into(),
            "login over clear" => b["tls"] = "none".into(),
            "bad SPF include" => b["spf_include"] = "not a name".into(),
            "unknown field" => b["extra"] = 1.into(),
            _ => {}
        }
        assert_eq!(put(b).await.status, 400, "{what}");
    }
    assert!(r.store.repos().relay.get(rusty_core::DomainId(id)).await.unwrap().is_none(), "nothing was stored");
    assert!(r.store.repos().sealed.meta("domain", id).await.unwrap().is_empty(), "no password was sealed");
}

#[tokio::test]
async fn without_a_sealing_key_a_login_cannot_be_stored_but_a_plain_relay_can() {
    let r = rig_without_sealing().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let id = domain_id(&r, "one.test").await;
    let e = c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("smtp.mail.net", Some("u"), Some("pw-12345")))).await;
    assert_eq!((e.status, e.json()["error"].clone()), (503, "unavailable".into()));
    assert!(r.store.repos().relay.get(rusty_core::DomainId(id)).await.unwrap().is_none());
    assert_eq!(c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("smtp.mail.net", None, None))).await.status, 200);
}

// ------------------------------------------------------------------------------------------------ isolation and guards

#[tokio::test]
async fn a_domain_administrator_cannot_reach_another_tenants_dkim_or_relay_and_cannot_tell_it_exists() {
    let r = rig().await;
    make_admin(&r, "boss@one.test", true).await;
    make_admin(&r, "boss@two.test", true).await;
    let two = domain_id(&r, "two.test").await;
    let other = Client::sign_in(r.addr, "boss@two.test").await;
    assert_eq!(other.post("/api/v1/domains/two.test/dkim", json!({})).await.status, 201);
    assert_eq!(other.mutate("PUT", "/api/v1/domains/two.test/relay", Some(relay_body("smtp.mail.net", Some("u"), Some("pw-12345")))).await.status, 200);
    let c = Client::sign_in(r.addr, "boss@one.test").await;
    let body = relay_body("evil.example.net", Some("u"), Some("pw-12345"));
    let calls: Vec<(&str, &str, &str, Option<serde_json::Value>)> = vec![
        ("GET", "/api/v1/domains/two.test/dkim", "/api/v1/domains/nowhere.test/dkim", None),
        ("POST", "/api/v1/domains/two.test/dkim", "/api/v1/domains/nowhere.test/dkim", Some(json!({ "selector": "x" }))),
        ("GET", "/api/v1/domains/two.test/relay", "/api/v1/domains/nowhere.test/relay", None),
        ("PUT", "/api/v1/domains/two.test/relay", "/api/v1/domains/nowhere.test/relay", Some(body.clone())),
        ("DELETE", "/api/v1/domains/two.test/relay", "/api/v1/domains/nowhere.test/relay", None),
        ("POST", "/api/v1/domains/two.test/relay/check", "/api/v1/domains/nowhere.test/relay/check", None),
    ];
    for (m, foreign, missing, b) in calls {
        let a = c.mutate(m, foreign, b.clone()).await;
        let z = c.mutate(m, missing, b).await;
        assert_eq!((a.status, a.body.as_str()), (404, "{\"error\":\"not_found\"}"), "{m} {foreign}");
        assert_eq!((a.status, &a.body), (z.status, &z.body), "{m} {foreign}: must not reveal that the tenant exists");
    }
    // Nothing of the other tenant changed: its relay is still the one it set, with its password.
    let theirs = r.store.repos().relay.get(rusty_core::DomainId(two)).await.unwrap().unwrap();
    assert_eq!(theirs.host, "smtp.mail.net");
    assert!(r.store.repos().sealed.get("domain", two, RELAY_PASSWORD_PURPOSE).await.unwrap().is_some());
    assert_eq!(r.count_jobs("relay.check").await, 0);
    // Its own domain works.
    assert_eq!(c.post("/api/v1/domains/one.test/dkim", json!({})).await.status, 201);
}

#[tokio::test]
async fn every_outbound_mutation_needs_all_three_csrf_guards_and_changes_nothing_without_them() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    c.mutate("PUT", "/api/v1/domains/one.test/relay", Some(relay_body("smtp.mail.net", None, None))).await;
    let id = domain_id(&r, "one.test").await;
    let evil = relay_body("evil.mail.net", None, None).to_string();
    let muts: Vec<(&str, &str, String)> = vec![
        ("POST", "/api/v1/domains/one.test/dkim", "{}".into()),
        ("PUT", "/api/v1/domains/one.test/relay", evil),
        ("DELETE", "/api/v1/domains/one.test/relay", String::new()),
        ("POST", "/api/v1/domains/one.test/relay/check", String::new()),
    ];
    for (m, p, b) in muts {
        let h = |origin: &'static str, ctype: &'static str, token: Option<&str>| {
            let mut v = vec![("Cookie", c.cookie.clone()), ("Origin", origin.to_owned()), ("Content-Type", ctype.to_owned())];
            if let Some(t) = token {
                v.push(("X-CSRF-Token", t.to_owned()));
            }
            v
        };
        async fn go(r: &Rig, m: &str, p: &str, b: &str, headers: Vec<(&str, String)>) -> u16 {
            let refs: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
            send(r.addr, m, p, &refs, b).await.status
        }
        assert_eq!(go(&r, m, p, &b, vec![("Origin", ORIGIN.into()), ("Content-Type", "application/json".into())]).await, 401, "{m} {p}: no session");
        assert_eq!(go(&r, m, p, &b, h(ORIGIN, "application/json", None)).await, 403, "{m} {p}: no token");
        assert_eq!(go(&r, m, p, &b, h(ORIGIN, "application/json", Some(&"0".repeat(64)))).await, 403, "{m} {p}: wrong token");
        assert_eq!(go(&r, m, p, &b, h("https://evil.example", "application/json", Some(&c.csrf))).await, 403, "{m} {p}: foreign origin");
        assert_eq!(go(&r, m, p, &b, h(ORIGIN, "text/plain", Some(&c.csrf))).await, 415, "{m} {p}: content type");
    }
    assert_eq!(r.store.repos().relay.get(rusty_core::DomainId(id)).await.unwrap().unwrap().host, "smtp.mail.net");
    assert!(r.store.repos().sealed.meta("domain", id).await.unwrap().is_empty());
    assert_eq!(r.count_jobs("relay.check").await, 0);
    // Reads need a session only.
    assert_eq!(send(r.addr, "GET", "/api/v1/domains/one.test/relay", &[], "").await.status, 401);
    assert_eq!(send(r.addr, "GET", "/api/v1/domains/one.test/dkim", &[], "").await.status, 401);
}

#[tokio::test]
async fn concurrent_requests_for_one_selector_make_exactly_one_key() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    c.post("/api/v1/domains", json!({ "name": "one.test" })).await;
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (addr, cookie, csrf) = (c.addr, c.cookie.clone(), c.csrf.clone());
        tasks.push(tokio::spawn(async move {
            let cl = Client { addr, cookie, csrf };
            cl.post("/api/v1/domains/one.test/dkim", json!({ "selector": "race" })).await
        }));
    }
    let mut won = Vec::new();
    let mut statuses = Vec::new();
    for t in tasks {
        let rep = t.await.unwrap();
        statuses.push(rep.status);
        if rep.status == 201 {
            won.push(rep.json()["record_value"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(statuses.iter().filter(|s| **s == 201).count(), 1, "{statuses:?}");
    assert!(statuses.iter().all(|s| *s == 201 || *s == 409), "{statuses:?}");
    // The published key is the winner's.
    let list = c.get("/api/v1/domains/one.test/dkim").await.json();
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["record_value"].as_str(), Some(won[0].as_str()));
    assert_eq!(list["items"][0]["sealed"], true);
}
