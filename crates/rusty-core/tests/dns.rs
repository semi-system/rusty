//! The DNS record generator: pure, structured, and every record round-trips through a zone line.

use std::net::IpAddr;

use proptest::prelude::*;
use rusty_core::dns::{
    parse_zone_line, records_for_domain, reverse_name, zone_line, DkimPublic, DnsInputs, RecordType, Requirement,
};

fn inputs<'a>(ips: &'a [IpAddr], dkim: Option<DkimPublic<'a>>, rua: Option<&'a str>) -> DnsInputs<'a> {
    DnsInputs { domain: "example.com", mail_host: "mail.example.com", outbound_ips: ips, dkim, dmarc_rua: rua }
}

fn find<'a>(recs: &'a [rusty_core::dns::DnsRecord], t: RecordType, name_prefix: &str) -> Vec<&'a rusty_core::dns::DnsRecord> {
    recs.iter().filter(|r| r.rtype == t && r.name.starts_with(name_prefix)).collect()
}

// backend-neutral: pure function, no database
#[test]
fn the_standard_set_for_a_domain() {
    let ips: Vec<IpAddr> = vec!["192.0.2.10".parse().unwrap(), "2001:db8::10".parse().unwrap()];
    let dkim = DkimPublic { selector: "rusty", public_key_b64: "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A" };
    let recs = records_for_domain(&inputs(&ips, Some(dkim), Some("dmarc@example.com"))).unwrap();
    let mx = find(&recs, RecordType::Mx, "example.com.");
    assert_eq!((mx.len(), mx[0].value.as_str(), mx[0].required), (1, "10 mail.example.com.", Requirement::Required));
    assert_eq!(find(&recs, RecordType::A, "mail.example.com.")[0].value, "192.0.2.10");
    assert_eq!(find(&recs, RecordType::Aaaa, "mail.example.com.")[0].value, "2001:db8::10");
    let spf = find(&recs, RecordType::Txt, "example.com.");
    assert_eq!(spf[0].value, "v=spf1 mx ip4:192.0.2.10 ip6:2001:db8::10 -all");
    let dk = find(&recs, RecordType::Txt, "rusty._domainkey.example.com.");
    assert_eq!(dk[0].value, "v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A");
    let dm = find(&recs, RecordType::Txt, "_dmarc.example.com.");
    assert_eq!(dm[0].value, "v=DMARC1; p=none; rua=mailto:dmarc@example.com");
    let ptr: Vec<_> = recs.iter().filter(|r| r.rtype == RecordType::Ptr).collect();
    assert_eq!(ptr.len(), 2);
    assert_eq!(ptr[0].name, "10.2.0.192.in-addr.arpa.");
    assert!(ptr.iter().all(|p| p.value == "mail.example.com."));
    let srv: Vec<_> = recs.iter().filter(|r| r.rtype == RecordType::Srv).map(|r| r.value.as_str()).collect();
    assert_eq!(srv, ["0 1 587 mail.example.com.", "0 1 465 mail.example.com.", "0 1 993 mail.example.com."]);
}

// backend-neutral: pure function, no database
#[test]
fn without_a_dkim_key_a_placeholder_says_how_to_make_one() {
    let recs = records_for_domain(&inputs(&[], None, None)).unwrap();
    let p = recs.iter().find(|r| r.name.contains("_domainkey")).unwrap();
    assert!(p.value.contains("rustyctl dkim generate") && p.required == Requirement::Recommended);
    let d = recs.iter().find(|r| r.name.starts_with("_dmarc")).unwrap();
    assert_eq!(d.value, "v=DMARC1; p=none");
}

// backend-neutral: pure function, no database
#[test]
fn reverse_names() {
    assert_eq!(reverse_name("192.0.2.1".parse().unwrap()), "1.2.0.192.in-addr.arpa.");
    assert_eq!(
        reverse_name("2001:db8::1".parse().unwrap()),
        "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa."
    );
}

// backend-neutral: pure function, no database
#[test]
fn bad_inputs_are_invalid_and_no_private_key_text_can_appear() {
    let mut i = inputs(&[], None, None);
    i.domain = "bad domain";
    assert!(records_for_domain(&i).is_err());
    let mut i = inputs(&[], Some(DkimPublic { selector: "s;x", public_key_b64: "AAAA" }), None);
    assert!(records_for_domain(&i).is_err());
    i = inputs(&[], Some(DkimPublic { selector: "s", public_key_b64: "-----BEGIN PRIVATE KEY-----" }), None);
    assert!(records_for_domain(&i).is_err(), "only base64 can be published as a key");
    i = inputs(&[], None, Some("a@b.c; evil=1"));
    assert!(records_for_domain(&i).is_err());
}

// backend-neutral: pure function, no database
#[test]
fn long_txt_values_are_split_and_rejoin() {
    let long = "A".repeat(700);
    let dkim = DkimPublic { selector: "s1", public_key_b64: &long };
    let recs = records_for_domain(&inputs(&[], Some(dkim), None)).unwrap();
    let r = recs.iter().find(|r| r.name.starts_with("s1._domainkey")).unwrap();
    let line = zone_line(r);
    assert!(line.matches("\" \"").count() >= 2, "{line}");
    assert_eq!(parse_zone_line(&line).unwrap().value, r.value);
}

fn label() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,9}"
}

proptest! {
    // backend-neutral: pure function, no database
    #[test]
    fn every_record_round_trips_through_a_zone_line(
        d1 in label(), d2 in label(), h in label(),
        v4 in proptest::collection::vec(any::<[u8; 4]>(), 0..3),
        v6 in proptest::collection::vec(any::<[u8; 16]>(), 0..2),
        key in "[A-Za-z0-9+/]{4,600}",
        sel in "[a-z][a-z0-9]{0,10}",
        with_key in any::<bool>(),
    ) {
        let domain = format!("{d1}.{d2}.test");
        let host = format!("{h}.{d2}.test");
        let ips: Vec<IpAddr> = v4.into_iter().map(IpAddr::from).chain(v6.into_iter().map(IpAddr::from)).collect();
        let dkim = with_key.then_some(DkimPublic { selector: &sel, public_key_b64: &key });
        let recs = records_for_domain(&DnsInputs {
            domain: &domain, mail_host: &host, outbound_ips: &ips, dkim, dmarc_rua: Some("rua@example.org"),
        }).unwrap();
        for r in &recs {
            let line = zone_line(r);
            prop_assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
            let p = parse_zone_line(&line).unwrap();
            prop_assert_eq!((&p.name, p.ttl, p.rtype, &p.value), (&r.name, r.ttl_hint, r.rtype, &r.value), "{}", line);
        }
    }

    // backend-neutral: pure function, no database
    #[test]
    fn the_parser_never_panics(s in "\\PC{0,200}") {
        let _ = parse_zone_line(&s);
    }
}
