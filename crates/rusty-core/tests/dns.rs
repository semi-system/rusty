//! The DNS record generator: pure, structured, and every record round-trips through a zone line.

use std::net::IpAddr;

use proptest::prelude::*;
use rusty_core::dns::{
    parse_zone_line, records_for_domain, reverse_name, zone_line, DkimKind, DkimPublic, DnsInputs, RecordType, Requirement,
};

fn inputs<'a>(ips: &'a [IpAddr], dkim: &'a [DkimPublic<'a>], rua: Option<&'a str>) -> DnsInputs<'a> {
    DnsInputs { domain: "example.com", mail_host: "mail.example.com", outbound_ips: ips, dkim, spf_includes: &[], dmarc_rua: rua }
}

fn find<'a>(recs: &'a [rusty_core::dns::DnsRecord], t: RecordType, name_prefix: &str) -> Vec<&'a rusty_core::dns::DnsRecord> {
    recs.iter().filter(|r| r.rtype == t && r.name.starts_with(name_prefix)).collect()
}

#[test]
fn the_standard_set_for_a_domain() {
    let ips: Vec<IpAddr> = vec!["192.0.2.10".parse().unwrap(), "2001:db8::10".parse().unwrap()];
    let dkim = DkimPublic { selector: "rusty", kind: DkimKind::Rsa, public_key_b64: "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A" };
    let recs = records_for_domain(&inputs(&ips, &[dkim], Some("dmarc@example.com"))).unwrap();
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

#[test]
fn without_a_dkim_key_a_placeholder_says_how_to_make_one() {
    let recs = records_for_domain(&inputs(&[], &[], None)).unwrap();
    let p = recs.iter().find(|r| r.name.contains("_domainkey")).unwrap();
    assert!(p.value.contains("rustyctl dkim generate") && p.required == Requirement::Recommended);
    let d = recs.iter().find(|r| r.name.starts_with("_dmarc")).unwrap();
    assert_eq!(d.value, "v=DMARC1; p=none");
}

#[test]
fn reverse_names() {
    assert_eq!(reverse_name("192.0.2.1".parse().unwrap()), "1.2.0.192.in-addr.arpa.");
    assert_eq!(
        reverse_name("2001:db8::1".parse().unwrap()),
        "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa."
    );
}

#[test]
fn bad_inputs_are_invalid_and_no_private_key_text_can_appear() {
    let mut i = inputs(&[], &[], None);
    i.domain = "bad domain";
    assert!(records_for_domain(&i).is_err());
    let mut i = inputs(&[], &[DkimPublic { selector: "s;x", kind: DkimKind::Rsa, public_key_b64: "AAAA" }], None);
    assert!(records_for_domain(&i).is_err());
    i = inputs(&[], &[DkimPublic { selector: "s", kind: DkimKind::Rsa, public_key_b64: "-----BEGIN PRIVATE KEY-----" }], None);
    assert!(records_for_domain(&i).is_err(), "only base64 can be published as a key");
    i = inputs(&[], &[], Some("a@b.c; evil=1"));
    assert!(records_for_domain(&i).is_err());
}

#[test]
fn long_txt_values_are_split_and_rejoin() {
    let long = "A".repeat(700);
    let dkim = DkimPublic { selector: "s1", kind: DkimKind::Rsa, public_key_b64: &long };
    let recs = records_for_domain(&inputs(&[], &[dkim], None)).unwrap();
    let r = recs.iter().find(|r| r.name.starts_with("s1._domainkey")).unwrap();
    let line = zone_line(r);
    assert!(line.matches("\" \"").count() >= 2, "{line}");
    assert_eq!(parse_zone_line(&line).unwrap().value, r.value);
}

fn label() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,9}"
}

proptest! {
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
        let keys: Vec<DkimPublic> = if with_key { vec![DkimPublic { selector: &sel, kind: DkimKind::Rsa, public_key_b64: &key }] } else { vec![] };
        let recs = records_for_domain(&DnsInputs {
            domain: &domain, mail_host: &host, outbound_ips: &ips, dkim: &keys, spf_includes: &[], dmarc_rua: Some("rua@example.org"),
        }).unwrap();
        for r in &recs {
            let line = zone_line(r);
            prop_assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
            let p = parse_zone_line(&line).unwrap();
            prop_assert_eq!((&p.name, p.ttl, p.rtype, &p.value), (&r.name, r.ttl_hint, r.rtype, &r.value), "{}", line);
        }
    }

    #[test]
    fn the_parser_never_panics(s in "\\PC{0,200}") {
        let _ = parse_zone_line(&s);
    }
}

#[test]
fn trailing_dot_inputs_are_refused_instead_of_producing_double_dot_names() {
    // The generator takes names WITHOUT a trailing dot and adds it itself; a dotted input used to give
    // `example.com..` (review U4 minor).
    let ips: Vec<IpAddr> = vec!["192.0.2.10".parse().unwrap()];
    for (domain, host) in [("example.com.", "mail.example.com"), ("example.com", "mail.example.com."), ("example.com.", "mail.example.com.")] {
        let i = DnsInputs { domain, mail_host: host, outbound_ips: &ips, dkim: &[], spf_includes: &[], dmarc_rua: None };
        assert!(records_for_domain(&i).is_err(), "{domain:?} {host:?}");
    }
    let ok = records_for_domain(&inputs(&ips, &[], None)).unwrap();
    assert!(ok.iter().all(|r| !r.name.contains("..") && !r.value.contains("..")));
}

#[test]
fn the_k_tag_follows_the_algorithm_of_the_key() {
    // Found while reading the code for the admin web: the record always said k=rsa, while `dkim generate` makes ed25519 keys.
    let ed = DkimPublic { selector: "rusty-ed25519", kind: DkimKind::Ed25519, public_key_b64: "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=" };
    let recs = records_for_domain(&inputs(&[], &[ed], None)).unwrap();
    let r = find(&recs, RecordType::Txt, "rusty-ed25519._domainkey.example.com.");
    assert_eq!(r[0].value, "v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=");
}

#[test]
fn a_domain_can_publish_an_ed25519_and_an_rsa_key_side_by_side() {
    let ed = DkimPublic { selector: "rusty-ed25519", kind: DkimKind::Ed25519, public_key_b64: "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=" };
    let rsa = DkimPublic { selector: "rusty", kind: DkimKind::Rsa, public_key_b64: "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A" };
    let recs = records_for_domain(&inputs(&[], &[ed, rsa], None)).unwrap();
    let keys: Vec<&str> = recs.iter().filter(|r| r.name.contains("._domainkey.")).map(|r| r.name.as_str()).collect();
    assert_eq!(keys, ["rusty-ed25519._domainkey.example.com.", "rusty._domainkey.example.com."], "in the order given");
    assert!(recs.iter().all(|r| !r.purpose.contains("not generated")), "no placeholder when a key exists");
    for r in recs.iter().filter(|r| r.name.contains("._domainkey.")) {
        assert_eq!(r.required, Requirement::Required);
    }
}

#[test]
fn without_a_key_there_is_one_placeholder_that_says_how_to_make_one() {
    let recs = records_for_domain(&inputs(&[], &[], None)).unwrap();
    let ph = recs.iter().filter(|r| r.name.contains("<selector>")).count();
    assert_eq!(ph, 1);
}

#[test]
fn duplicate_selectors_and_too_many_keys_are_refused() {
    let k = |sel: &'static str| DkimPublic { selector: sel, kind: DkimKind::Rsa, public_key_b64: "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A" };
    assert!(records_for_domain(&inputs(&[], &[k("a"), k("a")], None)).is_err(), "two keys under one selector cannot be published");
    assert!(records_for_domain(&inputs(&[], &[k("A"), k("a")], None)).is_err(), "selectors are case-insensitive in DNS");
    let five = [k("a"), k("b"), k("c"), k("d"), k("e")];
    assert!(records_for_domain(&inputs(&[], &five, None)).is_err());
    assert!(records_for_domain(&inputs(&[], &five[..4], None)).is_ok());
}

#[test]
fn a_relay_providers_spf_include_goes_before_the_all() {
    let ips: Vec<IpAddr> = vec!["192.0.2.10".parse().unwrap()];
    let mut i = inputs(&ips, &[], None);
    let inc = ["amazonses.com", "_spf.example.org"];
    i.spf_includes = &inc;
    let recs = records_for_domain(&i).unwrap();
    let spf = find(&recs, RecordType::Txt, "example.com.").into_iter().find(|r| r.value.starts_with("v=spf1")).unwrap();
    assert_eq!(spf.value, "v=spf1 mx ip4:192.0.2.10 include:amazonses.com include:_spf.example.org -all");
    // Without any: as before.
    let plain = records_for_domain(&inputs(&ips, &[], None)).unwrap();
    let spf = find(&plain, RecordType::Txt, "example.com.").into_iter().find(|r| r.value.starts_with("v=spf1")).unwrap();
    assert_eq!(spf.value, "v=spf1 mx ip4:192.0.2.10 -all");
}

#[test]
fn bad_or_too_many_spf_includes_are_refused() {
    let ips: Vec<IpAddr> = vec!["192.0.2.10".parse().unwrap()];
    for bad in ["", "a b", "x;y", "include:x.example", "x.example.", "-bad.example"] {
        let mut i = inputs(&ips, &[], None);
        let one = [bad];
        i.spf_includes = &one;
        assert!(records_for_domain(&i).is_err(), "{bad:?}");
    }
    let mut i = inputs(&ips, &[], None);
    let dup = ["x.example", "X.EXAMPLE"];
    i.spf_includes = &dup;
    assert!(records_for_domain(&i).is_err(), "the same include twice");
    let many = ["a.example", "b.example", "c.example", "d.example", "e.example", "f.example"];
    i.spf_includes = &many;
    assert!(records_for_domain(&i).is_err(), "at most five: each one costs a DNS lookup of the ten SPF allows");
}
