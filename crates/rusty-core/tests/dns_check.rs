//! The DNS validation engine (plan U10, W1): for each record of the generated set it asks DNS what is
//! published and says `ok`, `missing`, `mismatch` or `warning`, with the evidence. The source of DNS is a seam
//! (`DnsSource`); here it is a zone held in memory that starts as a perfect publication of the generated set and
//! is then broken one way at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusty_core::dns::{records_for_domain, DkimKind, DkimPublic, DnsInputs, DnsRecord, RecordType};
use rusty_core::dns_check::{check_records, CheckOptions, DnsFail, DnsSource, MxEntry, RecordCheck, SrvEntry};
use rusty_core::dns_state::RecordStatus;

const DOMAIN: &str = "example.net";
const HOST: &str = "mx.example.net";
const V4: &str = "192.0.2.10";
const V6: &str = "2001:db8::10";

fn key_b64(len: usize) -> String {
    "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA".chars().cycle().take(len).collect()
}

fn expected() -> Vec<DnsRecord> {
    let ips: Vec<IpAddr> = vec![V4.parse().unwrap(), V6.parse().unwrap()];
    let b64 = key_b64(392);
    records_for_domain(&DnsInputs {
        domain: DOMAIN,
        mail_host: HOST,
        outbound_ips: &ips,
        dkim: &[DkimPublic { selector: "rusty", kind: DkimKind::Rsa, public_key_b64: &b64 }],
        dmarc_rua: Some("dmarc@example.net"),
    })
    .unwrap()
}

#[derive(Default)]
struct Zone {
    txt: BTreeMap<String, Vec<String>>,
    mx: BTreeMap<String, Vec<MxEntry>>,
    addrs: BTreeMap<String, Vec<IpAddr>>,
    ptr: BTreeMap<IpAddr, Vec<String>>,
    srv: BTreeMap<String, Vec<SrvEntry>>,
    fail: BTreeSet<String>,
    queries: AtomicUsize,
}

fn bare(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

impl Zone {
    /// A perfect publication of `expected`.
    fn published(expected: &[DnsRecord]) -> Zone {
        let mut z = Zone::default();
        for r in expected {
            let name = bare(&r.name);
            match r.rtype {
                RecordType::Mx => {
                    let (pref, host) = r.value.split_once(' ').unwrap();
                    z.mx.entry(name).or_default().push(MxEntry { preference: pref.parse().unwrap(), host: bare(host) });
                }
                RecordType::A | RecordType::Aaaa => z.addrs.entry(name).or_default().push(r.value.parse().unwrap()),
                RecordType::Txt => z.txt.entry(name).or_default().push(r.value.clone()),
                RecordType::Srv => {
                    let p: Vec<&str> = r.value.split(' ').collect();
                    z.srv.entry(name).or_default().push(SrvEntry { priority: p[0].parse().unwrap(), weight: p[1].parse().unwrap(), port: p[2].parse().unwrap(), target: bare(p[3]) });
                }
                RecordType::Ptr => {}
            }
        }
        for ip in [V4, V6] {
            z.ptr.insert(ip.parse().unwrap(), vec![HOST.to_owned()]);
        }
        z
    }

    fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }

    fn set_txt(&mut self, name: &str, values: &[&str]) {
        self.txt.insert(name.to_owned(), values.iter().map(|s| (*s).to_owned()).collect());
    }
}

impl DnsSource for Zone {
    async fn txt(&self, name: &str) -> Result<Vec<String>, DnsFail> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self.fail.contains(name) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.txt.get(name).cloned().unwrap_or_default())
    }
    async fn mx(&self, name: &str) -> Result<Vec<MxEntry>, DnsFail> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self.fail.contains(name) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.mx.get(name).cloned().unwrap_or_default())
    }
    async fn addresses(&self, name: &str) -> Result<Vec<IpAddr>, DnsFail> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self.fail.contains(name) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.addrs.get(name).cloned().unwrap_or_default())
    }
    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, DnsFail> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self.fail.contains(&ip.to_string()) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.ptr.get(&ip).cloned().unwrap_or_default())
    }
    async fn srv(&self, name: &str) -> Result<Vec<SrvEntry>, DnsFail> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self.fail.contains(name) {
            return Err(DnsFail::Temporary);
        }
        Ok(self.srv.get(name).cloned().unwrap_or_default())
    }
}

async fn run(z: &Zone) -> Vec<RecordCheck> {
    check_records(z, &expected(), &CheckOptions::default()).await
}

fn find<'a>(checks: &'a [RecordCheck], rtype: RecordType, name_prefix: &str) -> Vec<&'a RecordCheck> {
    checks.iter().filter(|c| c.record.rtype == rtype && c.record.name.starts_with(name_prefix)).collect()
}

fn one<'a>(checks: &'a [RecordCheck], rtype: RecordType, name_prefix: &str) -> &'a RecordCheck {
    let v = find(checks, rtype, name_prefix);
    assert_eq!(v.len(), 1, "{rtype:?} {name_prefix}: {} matches", v.len());
    v[0]
}

fn spf(checks: &[RecordCheck]) -> &RecordCheck {
    checks.iter().find(|c| c.record.rtype == RecordType::Txt && c.record.value.starts_with("v=spf1")).unwrap()
}

fn has(c: &RecordCheck, code: &str) -> bool {
    c.findings.contains(&code)
}

// ----------------------------------------------------------------------------------- a perfect zone

#[tokio::test]
async fn a_perfect_publication_is_all_ok_and_cheap() {
    let z = Zone::published(&expected());
    let checks = run(&z).await;
    assert_eq!(checks.len(), expected().len());
    for c in &checks {
        assert_eq!(c.status, RecordStatus::Ok, "{} {:?}: {:?} {:?}", c.record.name, c.record.rtype, c.status, c.findings);
        assert!(c.findings.is_empty(), "{} {:?}", c.record.name, c.findings);
    }
    assert!(z.queries() <= 40, "{} queries", z.queries());
}

// ------------------------------------------------------------------------------------------- MX, A

#[tokio::test]
async fn mx_missing_pointing_elsewhere_and_pointing_at_us() {
    let mut z = Zone::published(&expected());
    z.mx.clear();
    let c = run(&z).await;
    assert_eq!(one(&c, RecordType::Mx, "example.net.").status, RecordStatus::Missing);

    z.mx.insert(DOMAIN.into(), vec![MxEntry { preference: 10, host: "mail.other.example".into() }]);
    let c = run(&z).await;
    let mx = one(&c, RecordType::Mx, "example.net.");
    assert_eq!(mx.status, RecordStatus::Mismatch);
    assert!(mx.seen.as_deref().unwrap().contains("mail.other.example"));

    // Several exchangers, ours among them: the record we generated is there.
    z.mx.insert(DOMAIN.into(), vec![MxEntry { preference: 5, host: "backup.example.org".into() }, MxEntry { preference: 10, host: HOST.into() }]);
    assert_eq!(one(&run(&z).await, RecordType::Mx, "example.net.").status, RecordStatus::Ok);
}

#[tokio::test]
async fn an_mx_host_that_does_not_resolve_is_reported() {
    let mut z = Zone::published(&expected());
    z.addrs.remove(HOST);
    let c = run(&z).await;
    assert!(has(one(&c, RecordType::Mx, "example.net."), "mx.host_unresolvable"));
    for a in find(&c, RecordType::A, "mx.example.net.") {
        assert_eq!(a.status, RecordStatus::Missing);
    }
}

#[tokio::test]
async fn the_mail_host_address_must_be_the_one_we_send_from() {
    let mut z = Zone::published(&expected());
    z.addrs.insert(HOST.into(), vec!["198.51.100.7".parse().unwrap(), V6.parse().unwrap()]);
    let c = run(&z).await;
    let a = one(&c, RecordType::A, "mx.example.net.");
    assert_eq!(a.status, RecordStatus::Mismatch);
    assert!(a.seen.as_deref().unwrap().contains("198.51.100.7"));
    assert_eq!(one(&c, RecordType::Aaaa, "mx.example.net.").status, RecordStatus::Ok);
}

// ----------------------------------------------------------------------------------------------- SPF

#[tokio::test]
async fn spf_absent_duplicated_and_malformed() {
    let mut z = Zone::published(&expected());
    z.txt.remove(DOMAIN);
    assert_eq!(spf(&run(&z).await).status, RecordStatus::Missing);

    z.set_txt(DOMAIN, &["v=spf1 mx -all", "v=spf1 ip4:192.0.2.10 -all"]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.multiple_records"));

    z.set_txt(DOMAIN, &["v=spf1 ip4:not-an-address -all"]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.syntax"));

    // A TXT record that merely mentions spf is not an SPF record.
    z.set_txt(DOMAIN, &["google-site-verification=abc", "some v=spf1 text"]);
    assert_eq!(spf(&run(&z).await).status, RecordStatus::Missing);
}

#[tokio::test]
async fn spf_is_judged_by_what_it_authorises_not_by_its_spelling() {
    let mut z = Zone::published(&expected());
    for ok in [
        "v=spf1 mx ip4:192.0.2.10 ip6:2001:db8::10 -all",
        "v=spf1 ip4:192.0.2.0/24 ip6:2001:db8::/32 -all",
        "v=spf1 mx -all", // the MX host resolves to both of our addresses
        "v=spf1 a:mx.example.net -all",
        "v=spf1 ip4:192.0.2.10 ip6:2001:db8::10 include:_spf.unrelated.example ~all",
        "V=SPF1 IP4:192.0.2.10 IP6:2001:db8::10 -ALL",
    ] {
        z.set_txt(DOMAIN, &[ok]);
        z.set_txt("_spf.unrelated.example", &["v=spf1 ip4:203.0.113.0/24 -all"]);
        let c = run(&z).await;
        assert_eq!(spf(&c).status, RecordStatus::Ok, "{ok}: {:?}", spf(&c).findings);
    }
    z.set_txt(DOMAIN, &["v=spf1 ip4:203.0.113.7 -all"]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.ip_not_authorised"));
    // Only one of the two addresses authorised.
    z.set_txt(DOMAIN, &["v=spf1 ip4:192.0.2.10 -all"]);
    assert!(has(spf(&run(&z).await), "spf.ip_not_authorised"));
}

#[tokio::test]
async fn spf_through_a_relay_providers_include() {
    let mut z = Zone::published(&expected());
    z.set_txt(DOMAIN, &["v=spf1 include:spf.relay.example -all"]);
    z.set_txt("spf.relay.example", &["v=spf1 include:_a.relay.example include:_b.relay.example ~all"]);
    z.set_txt("_a.relay.example", &["v=spf1 ip4:198.51.100.0/24 -all"]);
    z.set_txt("_b.relay.example", &["v=spf1 ip4:192.0.2.0/28 ip6:2001:db8::/64 -all"]);
    assert_eq!(spf(&run(&z).await).status, RecordStatus::Ok);
}

#[tokio::test]
async fn spf_redirect_is_followed() {
    let mut z = Zone::published(&expected());
    z.set_txt(DOMAIN, &["v=spf1 redirect=_spf.shared.example"]);
    z.set_txt("_spf.shared.example", &["v=spf1 ip4:192.0.2.10 ip6:2001:db8::10 -all"]);
    assert_eq!(spf(&run(&z).await).status, RecordStatus::Ok);
}

#[tokio::test]
async fn spf_over_ten_dns_lookups_is_a_permanent_error_and_the_check_stops_asking() {
    let mut z = Zone::published(&expected());
    let includes: Vec<String> = (0..12).map(|i| format!("include:i{i}.example")).collect();
    z.set_txt(DOMAIN, &[&format!("v=spf1 ip4:192.0.2.10 ip6:2001:db8::10 {} -all", includes.join(" "))]);
    for i in 0..12 {
        z.set_txt(&format!("i{i}.example"), &["v=spf1 ip4:203.0.113.0/24 -all"]);
    }
    let before = z.queries();
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.too_many_lookups"));
    assert!(z.queries() - before <= 40, "the evaluation is bounded");
}

#[tokio::test]
async fn an_include_loop_is_bounded() {
    let mut z = Zone::published(&expected());
    z.set_txt(DOMAIN, &["v=spf1 include:a.example -all"]);
    z.set_txt("a.example", &["v=spf1 include:b.example -all"]);
    z.set_txt("b.example", &["v=spf1 include:a.example -all"]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.too_many_lookups") || has(spf(&c), "spf.loop"));
    assert!(z.queries() < 80);
}

#[tokio::test]
async fn spf_all_qualifiers() {
    let mut z = Zone::published(&expected());
    let base = "ip4:192.0.2.10 ip6:2001:db8::10";
    z.set_txt(DOMAIN, &[&format!("v=spf1 {base} +all")]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.open"));
    z.set_txt(DOMAIN, &[&format!("v=spf1 {base} ?all")]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Warning);
    assert!(has(spf(&c), "spf.weak_all"));
    z.set_txt(DOMAIN, &[&format!("v=spf1 {base}")]);
    assert!(has(spf(&run(&z).await), "spf.no_all"));
    z.set_txt(DOMAIN, &[&format!("v=spf1 {base} ~all")]);
    assert_eq!(spf(&run(&z).await).status, RecordStatus::Ok);
}

#[tokio::test]
async fn spf_features_that_cannot_be_decided_are_a_warning_not_a_verdict() {
    let mut z = Zone::published(&expected());
    z.set_txt(DOMAIN, &["v=spf1 exists:%{i}._spf.example -all"]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Warning);
    assert!(has(spf(&c), "spf.macros_unsupported"));
    // A DNS failure while following an include: we cannot say.
    z.set_txt(DOMAIN, &["v=spf1 include:flaky.example -all"]);
    z.fail.insert("flaky.example".into());
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Warning);
    assert!(has(spf(&c), "dns.temporary"));
}

#[tokio::test]
async fn too_many_void_lookups_are_an_error() {
    let mut z = Zone::published(&expected());
    z.set_txt(DOMAIN, &["v=spf1 ip4:192.0.2.10 ip6:2001:db8::10 a:n1.example a:n2.example a:n3.example -all"]);
    let c = run(&z).await;
    assert_eq!(spf(&c).status, RecordStatus::Mismatch);
    assert!(has(spf(&c), "spf.too_many_void"));
}

// --------------------------------------------------------------------------------------------- DKIM

const DKIM_NAME: &str = "rusty._domainkey.example.net";

fn dkim(checks: &[RecordCheck]) -> &RecordCheck {
    one(checks, RecordType::Txt, "rusty._domainkey")
}

#[tokio::test]
async fn dkim_missing_different_revoked_and_short() {
    let mut z = Zone::published(&expected());
    z.txt.remove(DKIM_NAME);
    assert_eq!(dkim(&run(&z).await).status, RecordStatus::Missing);

    z.set_txt(DKIM_NAME, &[&format!("v=DKIM1; k=rsa; p={}", key_b64(392).replace('M', "N"))]);
    let c = run(&z).await;
    assert_eq!(dkim(&c).status, RecordStatus::Mismatch);
    assert!(has(dkim(&c), "dkim.key_differs"));

    z.set_txt(DKIM_NAME, &["v=DKIM1; k=rsa; p="]);
    assert!(has(dkim(&run(&z).await), "dkim.revoked"));

    z.set_txt(DKIM_NAME, &[&format!("v=DKIM1; k=rsa; p={}", key_b64(392)), &format!("v=DKIM1; k=rsa; p={}", key_b64(392))]);
    assert!(has(dkim(&run(&z).await), "dkim.multiple_records"));
}

#[tokio::test]
async fn dkim_tolerates_the_spelling_a_dns_host_may_produce() {
    let mut z = Zone::published(&expected());
    let p = key_b64(392);
    // Folded with spaces, tags in another order, without v=, with a trailing semicolon.
    let folded = format!("{} {}", &p[..100], &p[100..]);
    for text in [format!("k=rsa; p={folded}"), format!("p={p}; k=rsa;"), format!("v=DKIM1;k=rsa;p={p}"), format!("v=DKIM1; k=rsa; s=email; p={p}")] {
        z.set_txt(DKIM_NAME, &[&text]);
        let c = run(&z).await;
        assert_eq!(dkim(&c).status, RecordStatus::Ok, "{text:.40}: {:?}", dkim(&c).findings);
    }
}

#[tokio::test]
async fn dkim_warnings_testing_mode_and_a_short_rsa_key() {
    let mut z = Zone::published(&expected());
    let p = key_b64(392);
    z.set_txt(DKIM_NAME, &[&format!("v=DKIM1; k=rsa; t=y; p={p}")]);
    let c = run(&z).await;
    assert_eq!(dkim(&c).status, RecordStatus::Warning);
    assert!(has(dkim(&c), "dkim.testing"));
}

#[tokio::test]
async fn a_dkim_record_that_was_not_generated_yet_is_not_looked_up() {
    let ips: Vec<IpAddr> = vec![V4.parse().unwrap()];
    let no_key = records_for_domain(&DnsInputs { domain: DOMAIN, mail_host: HOST, outbound_ips: &ips, dkim: &[], dmarc_rua: None }).unwrap();
    let z = Zone::default();
    let c = check_records(&z, &no_key, &CheckOptions::default()).await;
    let k = c.iter().find(|c| c.record.name.starts_with("<selector>")).unwrap();
    assert_eq!(k.status, RecordStatus::Warning);
    assert!(has(k, "dkim.not_generated"));
    assert!(z.queries() >= 1, "the other records are still checked");
}

// -------------------------------------------------------------------------------------------- DMARC

fn dmarc(checks: &[RecordCheck]) -> &RecordCheck {
    one(checks, RecordType::Txt, "_dmarc.")
}

#[tokio::test]
async fn dmarc_absent_duplicated_malformed_and_stricter() {
    let mut z = Zone::published(&expected());
    z.txt.remove("_dmarc.example.net");
    assert_eq!(dmarc(&run(&z).await).status, RecordStatus::Missing);

    z.set_txt("_dmarc.example.net", &["v=DMARC1; p=none", "v=DMARC1; p=reject"]);
    assert!(has(dmarc(&run(&z).await), "dmarc.multiple_records"));

    for bad in ["v=DMARC1", "v=DMARC1; p=maybe", "p=none", "v=DMARC2; p=none", "v=DMARC1; pct=100"] {
        z.set_txt("_dmarc.example.net", &[bad]);
        let c = run(&z).await;
        assert_eq!(dmarc(&c).status, RecordStatus::Mismatch, "{bad}");
        assert!(has(dmarc(&c), "dmarc.syntax"), "{bad}");
    }
    for ok in ["v=DMARC1; p=quarantine; rua=mailto:r@example.net", "v=DMARC1;p=reject;pct=100;adkim=s", "v=DMARC1; p=none; sp=reject"] {
        z.set_txt("_dmarc.example.net", &[ok]);
        assert_eq!(dmarc(&run(&z).await).status, RecordStatus::Ok, "{ok}");
    }
    z.set_txt("_dmarc.example.net", &["v=DMARC1; p=none; rua=http://example.net/r"]);
    let c = run(&z).await;
    assert_eq!(dmarc(&c).status, RecordStatus::Warning);
    assert!(has(dmarc(&c), "dmarc.rua_invalid"));
}

// ---------------------------------------------------------------------------------------------- PTR

#[tokio::test]
async fn reverse_dns_must_be_there_match_and_be_forward_confirmed() {
    let ip4: IpAddr = V4.parse().unwrap();
    let mut z = Zone::published(&expected());
    z.ptr.remove(&ip4);
    let c = run(&z).await;
    let ptr4 = find(&c, RecordType::Ptr, "10.2.0.192.in-addr.arpa.");
    assert_eq!(ptr4[0].status, RecordStatus::Missing);

    z.ptr.insert(ip4, vec!["static-10.provider.example".into()]);
    let c = run(&z).await;
    assert_eq!(find(&c, RecordType::Ptr, "10.2.0.192")[0].status, RecordStatus::Mismatch);

    // The right name, but it does not point back to the address.
    z.ptr.insert(ip4, vec![HOST.into()]);
    z.addrs.insert(HOST.into(), vec![V6.parse().unwrap()]);
    let c = run(&z).await;
    let p = find(&c, RecordType::Ptr, "10.2.0.192")[0];
    assert_eq!(p.status, RecordStatus::Mismatch);
    assert!(has(p, "ptr.not_forward_confirmed"));
}

#[tokio::test]
async fn with_a_relay_the_reverse_records_are_not_checked_at_all() {
    let z = Zone::published(&expected());
    let c = check_records(&z, &expected(), &CheckOptions { via_relay: true, ..CheckOptions::default() }).await;
    assert!(c.iter().all(|c| c.record.rtype != RecordType::Ptr));
    assert_eq!(c.len(), expected().len() - 2);
}

// ---------------------------------------------------------------------------------------------- SRV

#[tokio::test]
async fn client_setup_records() {
    let mut z = Zone::published(&expected());
    z.srv.remove("_imaps._tcp.example.net");
    let c = run(&z).await;
    assert_eq!(one(&c, RecordType::Srv, "_imaps._tcp").status, RecordStatus::Missing);
    z.srv.insert("_imaps._tcp.example.net".into(), vec![SrvEntry { priority: 0, weight: 1, port: 143, target: HOST.into() }]);
    let c = run(&z).await;
    assert_eq!(one(&c, RecordType::Srv, "_imaps._tcp").status, RecordStatus::Mismatch);
    assert_eq!(one(&c, RecordType::Srv, "_submission._tcp").status, RecordStatus::Ok);
}

// ------------------------------------------------------------------------------- failures and bounds

#[tokio::test]
async fn a_dns_failure_makes_that_record_a_warning_and_leaves_the_others_alone() {
    let mut z = Zone::published(&expected());
    z.fail.insert("_dmarc.example.net".into());
    let c = run(&z).await;
    assert_eq!(dmarc(&c).status, RecordStatus::Warning);
    assert!(has(dmarc(&c), "dns.temporary"));
    assert_eq!(one(&c, RecordType::Mx, "example.net.").status, RecordStatus::Ok);
    assert_eq!(spf(&c).status, RecordStatus::Ok);
}

#[tokio::test]
async fn hostile_answers_are_bounded() {
    let mut z = Zone::published(&expected());
    // A thousand TXT records and one of a megabyte: the check reads a bounded number and does not choke.
    let mut many: Vec<String> = (0..1000).map(|i| format!("noise={i}")).collect();
    many.push("v=spf1 ip4:192.0.2.10 ip6:2001:db8::10 -all".into());
    z.txt.insert(DOMAIN.into(), many);
    let c = run(&z).await;
    assert!(matches!(spf(&c).status, RecordStatus::Ok | RecordStatus::Missing));
    z.set_txt(DOMAIN, &[&format!("v=spf1 {}", "ip4:192.0.2.1 ".repeat(100_000))]);
    let c = run(&z).await;
    assert_ne!(spf(&c).status, RecordStatus::Ok);
    z.set_txt("_dmarc.example.net", &[&"x".repeat(1 << 20)]);
    assert_eq!(dmarc(&run(&z).await).status, RecordStatus::Mismatch);
}

#[tokio::test]
async fn what_was_seen_is_evidence_without_control_characters() {
    let mut z = Zone::published(&expected());
    z.set_txt("_dmarc.example.net", &["v=DMARC1;\r\n p=none\u{7}; \u{1b}[31mred"]);
    let c = run(&z).await;
    let seen = dmarc(&c).seen.clone().unwrap_or_default();
    assert!(!seen.chars().any(|ch| ch.is_control()), "{seen:?}");
    assert!(seen.len() <= 512);
}

// -------------------------------------------------------------------------- the state-machine seam

#[tokio::test]
async fn an_observation_carries_the_key_and_a_hash_of_what_was_expected() {
    let z = Zone::published(&expected());
    let c = run(&z).await;
    let a = c[0].observation();
    assert_eq!(a.key.name, c[0].record.name.to_ascii_lowercase());
    assert_eq!(a.key.rtype, c[0].record.rtype.as_str());
    assert_eq!(a.status, RecordStatus::Ok);
    assert_eq!(a.expected_hash, c[0].observation().expected_hash, "stable");
    let mut other = c[0].clone();
    other.record.value.push('x');
    assert_ne!(other.observation().expected_hash, a.expected_hash, "a changed expectation changes the hash");
    // All the hashes of a set are different from one another.
    let hashes: BTreeSet<String> = c.iter().map(|c| c.observation().expected_hash).collect();
    assert_eq!(hashes.len(), c.len());
}
