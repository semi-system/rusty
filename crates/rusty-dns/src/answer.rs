//! What the name server says to a question (docs/design-authoritative-dns.md, section 7 step 1): a pure function from the zones and a question to the sections of the answer. There is no network here and no wire format;
//! the codec and the listeners come after, and every rule of the protocol that is about *what to answer* is tested here.
//!
//! The rules: a name that is not in a hosted zone is `REFUSED` (this server is never recursive); a name that is has an authoritative answer (`AA`); a name with no record and nothing below it is `NXDOMAIN`, a name
//! with records but not of that type (or an empty name between others) is `NOERROR` with no answer ("no data"), both with the zone's SOA in the authority section so that the negative answer can be cached
//! (RFC 2308); a `CNAME` is followed inside the zone; `ANY` gets one record set only (RFC 8482); there are no wildcards; names compare without regard to case.

use crate::zone::{in_zone, normal, RType, Rdata, Record, Zone, Zones};

/// The most `CNAME`s followed in one answer.
const MAX_CHAIN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub name: String,
    pub qtype: RType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rcode {
    NoError,
    NxDomain,
    /// Not a name this server answers for.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub rcode: Rcode,
    /// The `AA` flag: the answer comes from the zone itself.
    pub authoritative: bool,
    pub answers: Vec<Record>,
    pub authority: Vec<Record>,
    pub additional: Vec<Record>,
}

impl Answer {
    fn refused() -> Answer {
        Answer { rcode: Rcode::Refused, authoritative: false, answers: Vec::new(), authority: Vec::new(), additional: Vec::new() }
    }
}

/// The SOA record of a negative answer: its time to live is the smaller of the SOA's own and its `minimum` (RFC 2308 section 3).
fn negative_soa(zone: &Zone) -> Record {
    Record { name: zone.origin.clone(), ttl: zone.soa_ttl.min(zone.soa.minimum), rdata: Rdata::Soa(zone.soa.clone()) }
}

/// The addresses the answer's names point at, when they are in the zone (the "additional" section; a resolver would otherwise ask again).
fn glue(zone: &Zone, targets: Vec<String>, out: &mut Vec<Record>, answers: &[Record]) {
    for t in targets {
        for r in zone.at(&t) {
            if matches!(r.rdata.rtype(), RType::A | RType::Aaaa) && !out.contains(r) && !answers.contains(r) {
                out.push(r.clone());
            }
        }
    }
}

fn targets_of(records: &[Record]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match &r.rdata {
            Rdata::Mx { host, .. } => Some(host.clone()),
            Rdata::Ns(h) => Some(h.clone()),
            Rdata::Srv { target, .. } => Some(target.clone()),
            _ => None,
        })
        .collect()
}

pub fn answer(zones: &Zones, q: &Query) -> Answer {
    let name = normal(&q.name);
    let Some(zone) = zones.find(&name) else { return Answer::refused() };
    let mut out = Answer { rcode: Rcode::NoError, authoritative: true, answers: Vec::new(), authority: Vec::new(), additional: Vec::new() };
    let mut at = name;
    for _ in 0..MAX_CHAIN {
        let here = zone.at(&at);
        if here.is_empty() {
            // The name has nothing; if something is below it, it exists without data, otherwise it does not exist.
            if !zone.exists(&at) {
                out.rcode = Rcode::NxDomain;
            }
            out.authority.push(negative_soa(zone));
            return out;
        }
        if q.qtype != RType::Cname {
            if let Some(c) = here.iter().find(|r| r.rdata.rtype() == RType::Cname) {
                out.answers.push(c.clone());
                let Rdata::Cname(target) = &c.rdata else { return out };
                if !in_zone(target, &zone.origin) {
                    return out;
                }
                at = target.clone();
                continue;
            }
        }
        let wanted: Vec<Record> = match q.qtype {
            // One record set only (RFC 8482): the first type this name has, in the order of the types.
            RType::Any => {
                let first = here.iter().map(|r| r.rdata.rtype()).min();
                here.iter().filter(|r| Some(r.rdata.rtype()) == first).cloned().collect()
            }
            t => here.iter().filter(|r| r.rdata.rtype() == t).cloned().collect(),
        };
        if wanted.is_empty() {
            out.authority.push(negative_soa(zone));
            return out;
        }
        let targets = targets_of(&wanted);
        out.answers.extend(wanted);
        let answers = out.answers.clone();
        glue(zone, targets, &mut out.additional, &answers);
        return out;
    }
    // A chain that is longer than it should be (or loops): what was gathered so far is the answer.
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::Soa;
    use std::net::Ipv4Addr;

    fn zones() -> Zones {
        let soa = Soa { mname: "ns1.example.org".into(), rname: "hostmaster.example.org".into(), serial: 7, refresh: 3600, retry: 600, expire: 604_800, minimum: 300 };
        let mut z = Zone::new("Example.ORG.", soa, 3600);
        assert!(z.add("example.org", 3600, Rdata::Ns("ns1.example.org".into())));
        assert!(z.add("example.org", 3600, Rdata::Mx { preference: 10, host: "mail.example.org".into() }));
        assert!(z.add("example.org", 3600, Rdata::Txt(vec!["v=spf1 mx -all".into()])));
        assert!(z.add("mail.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 25))));
        assert!(z.add("ns1.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 53))));
        assert!(z.add("www.example.org", 300, Rdata::Cname("web.example.org".into())));
        assert!(z.add("web.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 80))));
        assert!(z.add("away.example.org", 300, Rdata::Cname("elsewhere.example.net".into())));
        assert!(z.add("_dmarc.example.org", 300, Rdata::Txt(vec!["v=DMARC1; p=none".into()])));
        assert!(z.add("a.b.deep.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 9))));
        let mut zs = Zones::new();
        zs.insert(z);
        zs
    }

    fn ask(name: &str, t: RType) -> Answer {
        answer(&zones(), &Query { name: name.into(), qtype: t })
    }

    #[test]
    fn a_name_outside_every_zone_is_refused_and_never_resolved() {
        let a = ask("example.net", RType::A);
        assert_eq!((a.rcode, a.authoritative, a.answers.len()), (Rcode::Refused, false, 0));
        assert_eq!(ask("notexample.org", RType::A).rcode, Rcode::Refused, "a longer label that ends the same way is not inside the zone");
        assert_eq!(ask("", RType::A).rcode, Rcode::Refused);
        assert_eq!(ask(".", RType::Ns).rcode, Rcode::Refused);
    }

    #[test]
    fn a_record_is_answered_with_the_authority_flag_and_names_compare_without_case() {
        let a = ask("MAIL.Example.Org.", RType::A);
        assert_eq!((a.rcode, a.authoritative), (Rcode::NoError, true));
        assert_eq!(a.answers.len(), 1);
        assert_eq!(a.answers[0].rdata, Rdata::A(Ipv4Addr::new(192, 0, 2, 25)));
    }

    #[test]
    fn a_name_without_that_type_is_no_data_with_the_soa_and_a_missing_name_is_nxdomain_with_it() {
        let nodata = ask("mail.example.org", RType::Mx);
        assert_eq!((nodata.rcode, nodata.answers.len(), nodata.authority.len()), (Rcode::NoError, 0, 1));
        let nx = ask("nothing.example.org", RType::A);
        assert_eq!((nx.rcode, nx.authoritative, nx.answers.len()), (Rcode::NxDomain, true, 0));
        let Rdata::Soa(s) = &nx.authority[0].rdata else { panic!("the SOA") };
        assert_eq!(s.serial, 7);
        assert_eq!(nx.authority[0].ttl, 300, "the smaller of the SOA ttl and its minimum");
        // A type this server does not hold is no data, not an error.
        assert_eq!(ask("example.org", RType::Other(99)).rcode, Rcode::NoError);
    }

    #[test]
    fn an_empty_name_between_others_exists_and_has_no_data() {
        let a = ask("deep.example.org", RType::A);
        assert_eq!((a.rcode, a.answers.len(), a.authority.len()), (Rcode::NoError, 0, 1));
        assert_eq!(ask("b.deep.example.org", RType::Txt).rcode, Rcode::NoError);
        assert_eq!(ask("x.deep.example.org", RType::A).rcode, Rcode::NxDomain);
    }

    #[test]
    fn there_are_no_wildcards() {
        let mut z = zones();
        let zone = z.find("example.org").unwrap().clone();
        let mut zone = zone;
        assert!(zone.add("*.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 1))));
        z.insert(zone);
        let a = answer(&z, &Query { name: "anything.example.org".into(), qtype: RType::A });
        assert_eq!((a.rcode, a.answers.len()), (Rcode::NxDomain, 0), "a star in the zone is a name of its own, never a wildcard");
    }

    #[test]
    fn a_cname_is_followed_inside_the_zone_and_left_alone_outside_it() {
        let a = ask("www.example.org", RType::A);
        assert_eq!(a.answers.len(), 2);
        assert_eq!(a.answers[0].rdata, Rdata::Cname("web.example.org".into()));
        assert_eq!(a.answers[1].rdata, Rdata::A(Ipv4Addr::new(192, 0, 2, 80)));
        let out = ask("away.example.org", RType::A);
        assert_eq!((out.rcode, out.answers.len()), (Rcode::NoError, 1));
        // Asking for the CNAME itself does not follow it.
        let own = ask("www.example.org", RType::Cname);
        assert_eq!(own.answers.len(), 1);
    }

    #[test]
    fn a_cname_to_a_missing_name_in_the_zone_is_nxdomain_with_the_cname_in_the_answer() {
        let mut z = zones();
        let mut zone = z.find("example.org").unwrap().clone();
        assert!(zone.add("dead.example.org", 300, Rdata::Cname("gone.example.org".into())));
        z.insert(zone);
        let a = answer(&z, &Query { name: "dead.example.org".into(), qtype: RType::A });
        assert_eq!((a.rcode, a.answers.len(), a.authority.len()), (Rcode::NxDomain, 1, 1));
    }

    #[test]
    fn a_cname_loop_ends() {
        let mut zone = zones().find("example.org").unwrap().clone();
        assert!(zone.add("p.example.org", 300, Rdata::Cname("q.example.org".into())));
        assert!(zone.add("q.example.org", 300, Rdata::Cname("p.example.org".into())));
        let mut z = Zones::new();
        z.insert(zone);
        let a = answer(&z, &Query { name: "p.example.org".into(), qtype: RType::A });
        assert!(a.answers.len() <= MAX_CHAIN);
    }

    #[test]
    fn a_cname_cannot_share_a_name_with_other_data() {
        let mut zone = zones().find("example.org").unwrap().clone();
        assert!(!zone.add("mail.example.org", 300, Rdata::Cname("x.example.org".into())), "a CNAME next to an A");
        assert!(!zone.add("www.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 2))), "an A next to a CNAME");
        assert!(!zone.add("www.example.org", 300, Rdata::Cname("y.example.org".into())), "a second CNAME");
        assert!(!zone.add("example.net", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 3))), "a name outside the zone");
    }

    #[test]
    fn mx_and_ns_answers_carry_the_addresses_of_their_hosts_when_the_zone_has_them() {
        let mx = ask("example.org", RType::Mx);
        assert_eq!(mx.answers.len(), 1);
        assert_eq!(mx.additional.len(), 1);
        assert_eq!(mx.additional[0].name, "mail.example.org");
        let ns = ask("example.org", RType::Ns);
        assert_eq!(ns.additional.len(), 1);
        assert_eq!(ns.additional[0].name, "ns1.example.org");
    }

    #[test]
    fn any_gets_one_record_set_only() {
        let a = ask("example.org", RType::Any);
        assert_eq!(a.rcode, Rcode::NoError);
        let kinds: std::collections::BTreeSet<RType> = a.answers.iter().map(|r| r.rdata.rtype()).collect();
        assert_eq!(kinds.len(), 1, "{kinds:?}");
        assert_eq!(ask("nothing.example.org", RType::Any).rcode, Rcode::NxDomain);
    }

    #[test]
    fn the_soa_and_the_apex_ns_are_there_from_the_start() {
        let a = ask("example.org", RType::Soa);
        assert_eq!(a.answers.len(), 1);
        assert_eq!(a.answers[0].ttl, 3600);
        let longest = {
            let mut z = zones();
            let sub = Zone::new("mail.example.org", Soa { mname: "ns9.example.org".into(), rname: "h.example.org".into(), serial: 1, refresh: 1, retry: 1, expire: 1, minimum: 1 }, 60);
            z.insert(sub);
            answer(&z, &Query { name: "mail.example.org".into(), qtype: RType::A })
        };
        assert_eq!((longest.rcode, longest.answers.len()), (Rcode::NoError, 0), "the zone of the longest origin answers, and it has no A there");
    }

    #[test]
    fn the_codes_of_the_types_are_the_wire_numbers_and_round_trip() {
        for t in [RType::A, RType::Aaaa, RType::Ns, RType::Cname, RType::Soa, RType::Mx, RType::Txt, RType::Srv, RType::Caa, RType::Any, RType::Other(99)] {
            assert_eq!(RType::from_code(t.code()), t);
        }
        assert_eq!((RType::A.code(), RType::Mx.code(), RType::Txt.code(), RType::Aaaa.code()), (1, 15, 16, 28));
    }

    proptest::proptest! {
        /// Whatever is asked, the answer follows the rules: a name outside the zone is refused, one inside never is; a positive answer has records and no SOA.
        #[test]
        fn any_question_gets_a_well_formed_answer(name in "[a-zA-Z0-9.*_-]{0,60}", t in 0u16..300) {
            let zs = zones();
            let a = answer(&zs, &Query { name: name.clone(), qtype: RType::from_code(t) });
            let inside = zs.find(&normal(&name)).is_some();
            proptest::prop_assert_eq!(a.rcode == Rcode::Refused, !inside);
            if a.rcode == Rcode::Refused {
                proptest::prop_assert!(a.answers.is_empty() && !a.authoritative);
            } else {
                proptest::prop_assert!(a.authoritative);
                if a.answers.iter().any(|r| r.rdata.rtype() != RType::Cname) {
                    proptest::prop_assert!(a.authority.is_empty());
                }
            }
        }
    }
}
