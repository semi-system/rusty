//! `rustyctl domain dns <domain>` (the records to publish, from the configuration and the keys in the database) and
//! `rustyctl domain check <domain>` (the same records against what DNS publishes, recorded as drift state). The DNS here
//! is a zone in memory that starts as a perfect publication of what `domain dns` printed and is then broken.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use common::*;
use rusty_core::dns_check::{DnsFail, DnsSource, MxEntry, SrvEntry};
use rusty_core::dns_state::RecordStatus;
use rusty_ctl::{Command, OutboundFacts};

const IP: &str = "203.0.113.5";
const IPS: [IpAddr; 1] = [IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5))];

#[derive(Default)]
struct Zone {
    txt: BTreeMap<String, Vec<String>>,
    mx: BTreeMap<String, Vec<MxEntry>>,
    addrs: BTreeMap<String, Vec<IpAddr>>,
    ptr: BTreeMap<IpAddr, Vec<String>>,
    srv: BTreeMap<String, Vec<SrvEntry>>,
    fail: BTreeSet<String>,
}

impl DnsSource for Zone {
    async fn txt(&self, name: &str) -> Result<Vec<String>, DnsFail> {
        if self.fail.contains(name) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.txt.get(name).cloned().unwrap_or_default())
    }
    async fn mx(&self, name: &str) -> Result<Vec<MxEntry>, DnsFail> {
        if self.fail.contains(name) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.mx.get(name).cloned().unwrap_or_default())
    }
    async fn addresses(&self, name: &str) -> Result<Vec<IpAddr>, DnsFail> {
        Ok(self.addrs.get(name).cloned().unwrap_or_default())
    }
    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, DnsFail> {
        Ok(self.ptr.get(&ip).cloned().unwrap_or_default())
    }
    async fn srv(&self, name: &str) -> Result<Vec<SrvEntry>, DnsFail> {
        Ok(self.srv.get(name).cloned().unwrap_or_default())
    }
}

fn facts(include: Option<&'static str>, via_relay: bool) -> OutboundFacts<'static> {
    OutboundFacts { public_ips: &IPS, via_relay, spf_include: include }
}

/// A perfect zone for `example.org` served by `mail.example.net` at 203.0.113.5, from the `domain dns` JSON.
fn published(dns: &serde_json::Value, ptr: bool) -> Zone {
    let mut z = Zone::default();
    for r in dns["records"].as_array().unwrap() {
        let name = r["name"].as_str().unwrap().trim_end_matches('.').to_owned();
        let value = r["value"].as_str().unwrap();
        match r["type"].as_str().unwrap() {
            "MX" => {
                let (p, h) = value.split_once(' ').unwrap();
                z.mx.entry(name).or_default().push(MxEntry { preference: p.parse().unwrap(), host: h.trim_end_matches('.').to_owned() });
            }
            "A" | "AAAA" => z.addrs.entry(name).or_default().push(value.parse().unwrap()),
            "TXT" if !name.starts_with('<') => z.txt.entry(name).or_default().push(value.to_owned()),
            "SRV" => {
                let p: Vec<&str> = value.split(' ').collect();
                z.srv.entry(name).or_default().push(SrvEntry { priority: p[0].parse().unwrap(), weight: p[1].parse().unwrap(), port: p[2].parse().unwrap(), target: p[3].trim_end_matches('.').to_owned() });
            }
            _ => {}
        }
    }
    if ptr {
        z.ptr.insert(IP.parse().unwrap(), vec!["mail.example.net".to_owned()]);
    }
    z
}

async fn setup(b: &Backend) -> (rusty_directory::DomainInfo, tempfile::TempDir) {
    b.run(Command::DomainAdd { name: "example.org".into() }, &[]).await.unwrap();
    let keys = tempfile::tempdir().unwrap();
    b.run_with_dkim(Command::DkimGenerate { domain: "example.org".into(), selector: "rusty".into() }, keys.path()).await.unwrap();
    (b.dir.find_domain("example.org").await.unwrap().unwrap(), keys)
}

fn dns_cmd() -> Command {
    Command::DomainDns { name: "example.org".into() }
}

// --------------------------------------------------------------------------------------------- domain dns

#[tokio::test]
async fn domain_dns_prints_the_records_with_the_addresses_the_include_and_the_stored_key() {
    for_each_backend(|b| async move {
        let _ = setup(&b).await;
        let out = b.run_dns(dns_cmd(), facts(Some("amazonses.com"), false)).await.unwrap();
        for want in [
            "example.org. 3600 IN MX 10 mail.example.net.",
            "mail.example.net. 3600 IN A 203.0.113.5",
            "v=spf1 mx ip4:203.0.113.5 include:amazonses.com -all",
            "rusty-ed25519._domainkey.example.org.",
            "k=ed25519",
            "_dmarc.example.org.",
        ] {
            assert!(out.text.contains(want), "{want}\n{}", out.text);
        }
        assert!(!out.text.contains("<selector>"), "the key exists, so no placeholder");
        assert!(!out.text.contains("PRIVATE KEY"));
        assert!(out.json["records"].as_array().unwrap().len() >= 8);
        assert_eq!(out.json["domain"], "example.org");
        assert!(out.json["warnings"].as_array().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn without_public_addresses_the_records_say_so_instead_of_pretending() {
    for_each_backend(|b| async move {
        let _ = setup(&b).await;
        let out = b.run_dns(dns_cmd(), OutboundFacts::default()).await.unwrap();
        assert_eq!(out.json["warnings"], serde_json::json!(["no_public_ips"]), "{}", out.text);
        assert!(out.text.contains("public_ips"), "{}", out.text);
    })
    .await;
}

#[tokio::test]
async fn domain_dns_needs_a_hostname_and_an_existing_domain() {
    for_each_backend(|b| async move {
        let _ = setup(&b).await;
        assert!(b.run_with_host(dns_cmd(), &[], None).await.is_err(), "no hostname: nothing to point the records at");
        let unknown = Command::DomainDns { name: "nope.test".into() };
        assert!(b.run_dns(unknown, facts(None, false)).await.is_err());
    })
    .await;
}

// ------------------------------------------------------------------------------------------- domain check

#[tokio::test]
async fn a_correct_publication_checks_out_and_is_recorded() {
    for_each_backend(|b| async move {
        let (dom, _k) = setup(&b).await;
        let dns = b.run_dns(dns_cmd(), facts(None, false)).await.unwrap().json;
        let z = published(&dns, true);
        let out = b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.json["ok"], true);
        for r in out.json["records"].as_array().unwrap() {
            assert_eq!(r["status"], "ok", "{r}");
        }
        // Recorded: one state row per record, all ok.
        let states = b.dns.states(dom.id).await.unwrap();
        assert_eq!(states.len(), out.json["records"].as_array().unwrap().len());
        assert!(states.iter().all(|s| s.status == RecordStatus::Ok));
        assert!(out.text.contains("example.org") && out.text.contains("ok"), "{}", out.text);
    })
    .await;
}

#[tokio::test]
async fn a_wrong_spf_fails_the_check_names_the_finding_and_records_the_change() {
    for_each_backend(|b| async move {
        let (dom, _k) = setup(&b).await;
        let dns = b.run_dns(dns_cmd(), facts(None, false)).await.unwrap().json;
        let mut z = published(&dns, true);
        b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        z.txt.insert("example.org".into(), vec!["v=spf1 ip4:198.51.100.7 -all".into()]);
        let out = b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        assert!(out.failed, "a required record is wrong: {}", out.text);
        assert_eq!(out.json["ok"], false);
        let spf = out.json["records"].as_array().unwrap().iter().find(|r| r["purpose"].as_str().unwrap().starts_with("SPF")).unwrap();
        assert_eq!(spf["status"], "mismatch");
        assert_eq!(spf["findings"], serde_json::json!(["spf.ip_not_authorised"]));
        assert_eq!(spf["change"], "ok -> mismatch", "the transition since the last check is reported");
        assert!(out.text.contains("spf.ip_not_authorised"), "{}", out.text);
        // The history has exactly that change.
        let ev = b.dns.events(dom.id, 0, 50).await.unwrap();
        assert!(ev.iter().any(|e| e.to == RecordStatus::Mismatch && e.from == Some(RecordStatus::Ok)), "{ev:?}");
        // Checking again without a change adds no event.
        let n = ev.len();
        b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        assert_eq!(b.dns.events(dom.id, 0, 50).await.unwrap().len(), n);
    })
    .await;
}

#[tokio::test]
async fn a_record_that_cannot_be_checked_is_a_warning_not_a_failure() {
    for_each_backend(|b| async move {
        let _ = setup(&b).await;
        let dns = b.run_dns(dns_cmd(), facts(None, false)).await.unwrap().json;
        let mut z = published(&dns, true);
        z.fail.insert("_dmarc.example.org".into());
        let out = b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        let dmarc = out.json["records"].as_array().unwrap().iter().find(|r| r["name"].as_str().unwrap().starts_with("_dmarc")).unwrap();
        assert_eq!(dmarc["status"], "warning");
        assert_eq!(dmarc["findings"], serde_json::json!(["dns.temporary"]));
        assert!(!out.failed, "an undecidable recommended record does not fail the run: {}", out.text);
    })
    .await;
}

#[tokio::test]
async fn a_missing_required_record_fails_but_a_missing_optional_one_does_not() {
    for_each_backend(|b| async move {
        let _ = setup(&b).await;
        let dns = b.run_dns(dns_cmd(), facts(None, false)).await.unwrap().json;
        let mut z = published(&dns, true);
        z.srv.clear();
        let out = b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        assert!(!out.failed, "the SRV records are optional: {}", out.text);
        z.mx.clear();
        let out = b.check_with(&z, facts(None, false), "example.org").await.unwrap();
        assert!(out.failed, "{}", out.text);
    })
    .await;
}

#[tokio::test]
async fn through_a_relay_the_reverse_records_are_left_out() {
    for_each_backend(|b| async move {
        let _ = setup(&b).await;
        let with = b.run_dns(dns_cmd(), facts(Some("amazonses.com"), true)).await.unwrap().json;
        let z = published(&with, false);
        z.txt.get("example.org").unwrap();
        let out = b.check_with(&z, facts(Some("amazonses.com"), true), "example.org").await.unwrap();
        assert!(out.json["records"].as_array().unwrap().iter().all(|r| r["type"] != "PTR"), "{}", out.text);
    })
    .await;
}

#[tokio::test]
async fn checking_an_unknown_domain_is_an_error_and_records_nothing() {
    for_each_backend(|b| async move {
        let (dom, _k) = setup(&b).await;
        let z = Zone::default();
        assert!(b.check_with(&z, facts(None, false), "nope.test").await.is_err());
        assert!(b.dns.states(dom.id).await.unwrap().is_empty());
    })
    .await;
}
