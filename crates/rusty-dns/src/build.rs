//! From the records a domain needs (`rusty_core::dns::records_for_domain`, the one source of truth the DNS page also uses) to a zone the answer engine serves (docs/design-authoritative-dns.md, section 2): the
//! values are parsed into typed records, a TXT longer than 255 bytes is split into the strings the wire format wants, the SOA and the name servers are added, and what cannot be served is left out (a placeholder
//! that says how to make a DKIM key, a reverse name that belongs to the owner of the address block).

use rusty_core::dns::{DnsRecord, RecordType};

use crate::zone::{normal, Rdata, Soa, Zone};

/// What a zone needs besides the records of the domain.
#[derive(Debug, Clone)]
pub struct ZoneSpec<'a> {
    pub origin: &'a str,
    /// The name servers of the platform (`ns1.platform.example`), at least one.
    pub name_servers: &'a [String],
    /// The mailbox of the person responsible, in the SOA form (`hostmaster.platform.example`).
    pub hostmaster: &'a str,
    /// The zone's serial: it must grow when the zone changes (the caller keeps a counter or a date).
    pub serial: u32,
    pub ttl: u32,
}

/// A TXT value as the strings of one record: at most 255 bytes each, cut at a character boundary.
fn txt_strings(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in value.chars() {
        if cur.len() + c.len_utf8() > 255 {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn host(s: &str) -> String {
    normal(s)
}

/// One record of the domain as zone data; `None` for what a zone cannot serve.
fn rdata(r: &DnsRecord) -> Option<Rdata> {
    match r.rtype {
        RecordType::A => r.value.parse().ok().map(Rdata::A),
        RecordType::Aaaa => r.value.parse().ok().map(Rdata::Aaaa),
        RecordType::Txt => Some(Rdata::Txt(txt_strings(&r.value))),
        RecordType::Mx => {
            let (pref, target) = r.value.split_once(' ')?;
            Some(Rdata::Mx { preference: pref.parse().ok()?, host: host(target) })
        }
        RecordType::Srv => {
            let mut it = r.value.split(' ');
            let (priority, weight, port, target) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?);
            if it.next().is_some() {
                return None;
            }
            Some(Rdata::Srv { priority, weight, port, target: host(target) })
        }
        // A reverse name belongs to the owner of the address block, not to this zone.
        RecordType::Ptr => None,
    }
}

/// The zone of a hosted domain. Records whose names are outside the origin or are placeholders are left out, and so is any record that would break the CNAME rules (the zone refuses it).
pub fn build_zone(spec: &ZoneSpec<'_>, records: &[DnsRecord]) -> Zone {
    let soa = Soa {
        mname: host(spec.name_servers.first().map_or("", String::as_str)),
        rname: host(spec.hostmaster),
        serial: spec.serial,
        refresh: 3600,
        retry: 600,
        expire: 1_209_600,
        minimum: 300,
    };
    let mut z = Zone::new(spec.origin, soa, spec.ttl);
    for ns in spec.name_servers {
        z.add(spec.origin, spec.ttl, Rdata::Ns(host(ns)));
    }
    for r in records {
        if r.name.contains('<') {
            continue;
        }
        if let Some(data) = rdata(r) {
            z.add(&r.name, r.ttl_hint, data);
        }
    }
    z
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::{answer, Query, Rcode};
    use crate::zone::{RType, Zones};
    use rusty_core::dns::{records_for_domain, DkimKind, DkimPublic, DnsInputs};
    use std::net::IpAddr;

    fn spec<'a>(ns: &'a [String]) -> ZoneSpec<'a> {
        ZoneSpec { origin: "example.org", name_servers: ns, hostmaster: "hostmaster.platform.example", serial: 42, ttl: 3600 }
    }

    fn zone_of_example() -> Zones {
        let ips: Vec<IpAddr> = vec!["192.0.2.25".parse().unwrap(), "2001:db8::25".parse().unwrap()];
        let key = "A".repeat(400);
        let dkim = [DkimPublic { selector: "s1", kind: DkimKind::Rsa, public_key_b64: &key }];
        let recs = records_for_domain(&DnsInputs { domain: "example.org", mail_host: "mail.example.org", outbound_ips: &ips, dkim: &dkim, spf_includes: &[], dmarc_rua: None }).unwrap();
        let ns = vec!["ns1.platform.example".to_owned(), "ns2.platform.example".to_owned()];
        let mut zs = Zones::new();
        zs.insert(build_zone(&spec(&ns), &recs));
        zs
    }

    fn ask(zs: &Zones, name: &str, t: RType) -> crate::answer::Answer {
        answer(zs, &Query { name: name.into(), qtype: t })
    }

    #[test]
    fn the_records_a_domain_needs_are_what_the_server_says() {
        let zs = zone_of_example();
        let mx = ask(&zs, "example.org", RType::Mx);
        assert!(matches!(&mx.answers[0].rdata, Rdata::Mx { preference: 10, host } if host == "mail.example.org"));
        assert_eq!(mx.additional.len(), 2, "both addresses of the mail host ride along");
        assert_eq!(ask(&zs, "mail.example.org", RType::A).answers.len(), 1);
        assert_eq!(ask(&zs, "mail.example.org", RType::Aaaa).answers.len(), 1);
        let ns = ask(&zs, "example.org", RType::Ns);
        assert_eq!(ns.answers.len(), 2);
        let soa = ask(&zs, "example.org", RType::Soa);
        assert!(matches!(&soa.answers[0].rdata, Rdata::Soa(s) if s.serial == 42 && s.mname == "ns1.platform.example" && s.rname == "hostmaster.platform.example"));
        let srv = ask(&zs, "_imaps._tcp.example.org", RType::Srv);
        assert!(matches!(&srv.answers[0].rdata, Rdata::Srv { port: 993, target, .. } if target == "mail.example.org"));
        assert_eq!(ask(&zs, "_dmarc.example.org", RType::Txt).answers.len(), 1);
        assert_eq!(ask(&zs, "autoconfig.example.org", RType::A).answers.len(), 1);
    }

    #[test]
    fn a_long_txt_is_split_in_strings_of_at_most_255_bytes_and_joins_back() {
        let zs = zone_of_example();
        let a = ask(&zs, "s1._domainkey.example.org", RType::Txt);
        let Rdata::Txt(parts) = &a.answers[0].rdata else { panic!("TXT") };
        assert!(parts.len() >= 2 && parts.iter().all(|p| p.len() <= 255));
        assert!(parts.concat().starts_with("v=DKIM1; k=rsa; p=AAAA") && parts.concat().len() > 400);
        assert_eq!(txt_strings(""), [""]);
        assert_eq!(txt_strings(&"é".repeat(200)).iter().map(String::len).collect::<Vec<_>>(), [254, 146], "cut at a character, never inside one");
    }

    #[test]
    fn placeholders_reverse_names_and_names_outside_the_zone_are_left_out() {
        let recs = records_for_domain(&DnsInputs { domain: "example.org", mail_host: "mail.example.org", outbound_ips: &["192.0.2.25".parse().unwrap()], dkim: &[], spf_includes: &[], dmarc_rua: None }).unwrap();
        assert!(recs.iter().any(|r| r.name.contains('<')) && recs.iter().any(|r| r.rtype == RecordType::Ptr));
        let ns = vec!["ns1.platform.example".to_owned()];
        let z = build_zone(&spec(&ns), &recs);
        assert!(!z.all().any(|r| r.name.contains('<') || r.name.ends_with("in-addr.arpa")));
        let other = ZoneSpec { origin: "example.net", ..spec(&ns) };
        let z = build_zone(&other, &recs);
        assert_eq!(z.all().count(), 2, "only the SOA and the NS: everything else is outside example.net");
    }

    #[test]
    fn a_name_outside_the_hosted_zones_is_refused() {
        let zs = zone_of_example();
        assert_eq!(ask(&zs, "example.net", RType::A).rcode, Rcode::Refused);
        assert_eq!(ask(&zs, "nothing.example.org", RType::A).rcode, Rcode::NxDomain);
    }
}
