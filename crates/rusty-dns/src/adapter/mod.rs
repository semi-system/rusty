//! The DNS wire format (docs/design-authoritative-dns.md, section 2): the only module that names the codec library. A packet from the internet becomes a [`Parsed`] (a question, or a reason to answer with an error, or
//! nothing at all), and an [`Answer`] of the answer engine becomes the bytes to send, cut to what the client can take (a truncated UDP answer sets `TC`, and the client asks again over TCP).

use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query as WireQuery, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA, CNAME, MX, NS, SOA, SRV, TXT};
use hickory_proto::rr::{DNSClass, Name, RData, Record as WireRecord};

use crate::answer::{Answer, Rcode};
use crate::zone::{normal, Rdata, Record};

/// The most a UDP answer carries, whatever the client says it can take (a size that avoids fragmentation; RFC 9715).
pub const MAX_UDP_PAYLOAD: u16 = 1232;
/// What a client without EDNS can take over UDP (RFC 1035).
const CLASSIC_UDP_PAYLOAD: u16 = 512;

/// One question, as the server needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: u16,
    /// Lower case, without the trailing dot.
    pub name: String,
    /// The type number as asked.
    pub qtype: u16,
    pub recursion_desired: bool,
    /// The biggest UDP answer the client can take, capped by [`MAX_UDP_PAYLOAD`].
    pub udp_payload: u16,
    /// The query carried an EDNS record: the answer carries one too.
    pub edns: bool,
}

/// An error answer to a packet that is not a usable question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The packet could not be understood.
    FormErr,
    /// Not a standard query (an update, a notify, a status request).
    NotImp,
    /// An EDNS version this server does not speak.
    BadVers,
    /// A class this server does not serve (it is `IN` only).
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Query(Question),
    /// Answer with this error (the id of the packet, when it had one).
    Error(u16, Error),
    /// Say nothing: it is not a packet worth an answer (too short to have an id, or itself an answer: replying to answers is how reflection loops start).
    Drop,
}

/// Read a packet.
pub fn parse(bytes: &[u8]) -> Parsed {
    let Some(id) = bytes.get(0..2).map(|b| u16::from_be_bytes([b[0], b[1]])) else { return Parsed::Drop };
    // The QR bit: an answer is not a question.
    if bytes.get(2).is_some_and(|b| b & 0x80 != 0) {
        return Parsed::Drop;
    }
    let Ok(msg) = Message::from_vec(bytes) else { return Parsed::Error(id, Error::FormErr) };
    if msg.metadata.message_type != MessageType::Query {
        return Parsed::Drop;
    }
    if msg.metadata.op_code != OpCode::Query {
        return Parsed::Error(id, Error::NotImp);
    }
    let [q] = msg.queries.as_slice() else { return Parsed::Error(id, Error::FormErr) };
    if q.query_class() != DNSClass::IN {
        return Parsed::Error(id, Error::Refused);
    }
    let (udp_payload, edns) = match &msg.edns {
        Some(e) => {
            if e.version() > 0 {
                return Parsed::Error(id, Error::BadVers);
            }
            (e.max_payload().clamp(CLASSIC_UDP_PAYLOAD, MAX_UDP_PAYLOAD), true)
        }
        None => (CLASSIC_UDP_PAYLOAD, false),
    };
    Parsed::Query(Question { id, name: normal(&q.name().to_ascii()), qtype: u16::from(q.query_type()), recursion_desired: msg.metadata.recursion_desired, udp_payload, edns })
}

fn name(s: &str) -> Option<Name> {
    Name::from_ascii(format!("{s}.")).ok()
}

fn record(r: &Record) -> Option<WireRecord> {
    let data = match &r.rdata {
        Rdata::A(a) => RData::A(A(*a)),
        Rdata::Aaaa(a) => RData::AAAA(AAAA(*a)),
        Rdata::Ns(h) => RData::NS(NS(name(h)?)),
        Rdata::Cname(h) => RData::CNAME(CNAME(name(h)?)),
        Rdata::Soa(s) => RData::SOA(SOA::new(
            name(&s.mname)?,
            name(&s.rname)?,
            s.serial,
            i32::try_from(s.refresh).unwrap_or(i32::MAX),
            i32::try_from(s.retry).unwrap_or(i32::MAX),
            i32::try_from(s.expire).unwrap_or(i32::MAX),
            s.minimum,
        )),
        Rdata::Mx { preference, host } => RData::MX(MX::new(*preference, name(host)?)),
        Rdata::Txt(parts) => RData::TXT(TXT::new(parts.clone())),
        Rdata::Srv { priority, weight, port, target } => RData::SRV(SRV::new(*priority, *weight, *port, name(target)?)),
        // CAA is not served yet: the zones do not produce it.
        Rdata::Caa { .. } => return None,
    };
    Some(WireRecord::from_rdata(name(&r.name)?, r.ttl, data))
}

fn code(rc: Rcode) -> ResponseCode {
    match rc {
        Rcode::NoError => ResponseCode::NoError,
        Rcode::NxDomain => ResponseCode::NXDomain,
        Rcode::Refused => ResponseCode::Refused,
    }
}

fn edns() -> Edns {
    let mut e = Edns::new();
    e.set_max_payload(MAX_UDP_PAYLOAD);
    e.set_version(0);
    e
}

/// The bytes of the answer to `q`. With `limit` (UDP: `q.udp_payload`) a message that is bigger loses its records and carries `TC`; over TCP pass `None`.
pub fn encode(q: &Question, a: &Answer, limit: Option<usize>) -> Vec<u8> {
    let mut msg = Message::response(q.id, OpCode::Query);
    msg.metadata.authoritative = a.authoritative;
    msg.metadata.recursion_desired = q.recursion_desired;
    msg.metadata.recursion_available = false;
    msg.metadata.response_code = code(a.rcode);
    if let Some(n) = name(&q.name) {
        msg.add_query(WireQuery::query(n, q.qtype.into()));
    }
    for r in a.answers.iter().filter_map(record) {
        msg.add_answer(r);
    }
    for r in a.authority.iter().filter_map(record) {
        msg.add_authority(r);
    }
    for r in a.additional.iter().filter_map(record) {
        msg.add_additional(r);
    }
    if q.edns {
        msg.set_edns(edns());
    }
    let bytes = msg.to_vec().unwrap_or_default();
    match limit {
        Some(max) if bytes.len() > max => {
            msg.answers.clear();
            msg.authorities.clear();
            msg.additionals.clear();
            msg.metadata.truncation = true;
            msg.to_vec().unwrap_or_default()
        }
        _ => bytes,
    }
}

/// A question for the health check of the service: it asks for a name no zone holds, so any reply at all (the answer is `REFUSED`) shows that the server is up and reading packets.
pub fn probe_question() -> Vec<u8> {
    let mut m = Message::query();
    m.metadata.id = 0x5eed;
    if let Ok(n) = Name::from_ascii("health.invalid.") {
        m.add_query(WireQuery::query(n, hickory_proto::rr::RecordType::A));
    }
    m.to_vec().unwrap_or_default()
}

/// The bytes of an error answer to a packet.
pub fn encode_error(id: u16, e: Error) -> Vec<u8> {
    let rc = match e {
        Error::FormErr => ResponseCode::FormErr,
        Error::NotImp => ResponseCode::NotImp,
        Error::BadVers => ResponseCode::BADVERS,
        Error::Refused => ResponseCode::Refused,
    };
    let mut msg = Message::error_msg(id, OpCode::Query, rc);
    if e == Error::BadVers {
        msg.set_edns(edns());
    }
    msg.to_vec().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::{answer, Query};
    use crate::zone::{RType, Soa, Zone, Zones};
    use std::net::Ipv4Addr;

    fn zones() -> Zones {
        let soa = Soa { mname: "ns1.example.org".into(), rname: "hostmaster.example.org".into(), serial: 7, refresh: 3600, retry: 600, expire: 604_800, minimum: 300 };
        let mut z = Zone::new("example.org", soa, 3600);
        z.add("example.org", 3600, Rdata::Ns("ns1.example.org".into()));
        z.add("example.org", 3600, Rdata::Mx { preference: 10, host: "mail.example.org".into() });
        z.add("example.org", 3600, Rdata::Txt(vec!["v=spf1 mx -all".into()]));
        z.add("mail.example.org", 300, Rdata::A(Ipv4Addr::new(192, 0, 2, 25)));
        z.add("_imaps._tcp.example.org", 300, Rdata::Srv { priority: 0, weight: 1, port: 993, target: "mail.example.org".into() });
        for i in 0..40 {
            z.add("big.example.org", 300, Rdata::Txt(vec![format!("{i:0>200}")]));
        }
        let mut zs = Zones::new();
        zs.insert(z);
        zs
    }

    fn query_bytes(name: &str, t: hickory_proto::rr::RecordType, edns: Option<u16>) -> Vec<u8> {
        let mut m = Message::query();
        m.metadata.id = 0x1234;
        m.metadata.recursion_desired = true;
        m.add_query(WireQuery::query(Name::from_ascii(name).unwrap(), t));
        if let Some(p) = edns {
            let mut e = Edns::new();
            e.set_max_payload(p);
            m.set_edns(e);
        }
        m.to_vec().unwrap()
    }

    fn serve(bytes: &[u8], limit_udp: bool) -> Message {
        let Parsed::Query(q) = parse(bytes) else { panic!("a question") };
        let a = answer(&zones(), &Query { name: q.name.clone(), qtype: RType::from_code(q.qtype) });
        Message::from_vec(&encode(&q, &a, limit_udp.then_some(usize::from(q.udp_payload)))).unwrap()
    }

    #[test]
    fn a_question_is_parsed_and_its_answer_is_what_a_resolver_reads() {
        let m = serve(&query_bytes("Mail.Example.ORG.", hickory_proto::rr::RecordType::A, Some(1400)), true);
        assert_eq!((m.metadata.id, m.metadata.message_type, m.metadata.authoritative, m.metadata.recursion_available), (0x1234, MessageType::Response, true, false));
        assert!(m.metadata.recursion_desired, "RD is copied");
        assert_eq!(m.metadata.response_code, ResponseCode::NoError);
        assert_eq!(m.answers.len(), 1);
        assert_eq!(m.answers[0].data, RData::A(A(Ipv4Addr::new(192, 0, 2, 25))));
        assert!(m.edns.is_some(), "a query with EDNS gets EDNS back");
        assert_eq!(m.queries.len(), 1);
    }

    #[test]
    fn every_record_type_the_zones_make_goes_on_the_wire() {
        let mx = serve(&query_bytes("example.org.", hickory_proto::rr::RecordType::MX, None), true);
        assert!(matches!(mx.answers[0].data, RData::MX(ref m) if m.preference == 10 && m.exchange.to_ascii() == "mail.example.org."));
        assert_eq!(mx.additionals.len(), 1);
        let txt = serve(&query_bytes("example.org.", hickory_proto::rr::RecordType::TXT, None), true);
        assert!(matches!(txt.answers[0].data, RData::TXT(_)));
        let srv = serve(&query_bytes("_imaps._tcp.example.org.", hickory_proto::rr::RecordType::SRV, None), true);
        assert!(matches!(srv.answers[0].data, RData::SRV(ref s) if s.port == 993));
        let soa = serve(&query_bytes("example.org.", hickory_proto::rr::RecordType::SOA, None), true);
        assert!(matches!(soa.answers[0].data, RData::SOA(ref s) if s.serial == 7 && s.minimum == 300));
        let nx = serve(&query_bytes("nothing.example.org.", hickory_proto::rr::RecordType::A, None), true);
        assert_eq!((nx.metadata.response_code, nx.authorities.len()), (ResponseCode::NXDomain, 1));
        let refused = serve(&query_bytes("example.net.", hickory_proto::rr::RecordType::A, None), true);
        assert_eq!((refused.metadata.response_code, refused.metadata.authoritative), (ResponseCode::Refused, false));
    }

    #[test]
    fn a_big_answer_over_udp_is_cut_and_marked_and_whole_over_tcp() {
        let big = query_bytes("big.example.org.", hickory_proto::rr::RecordType::TXT, None);
        let udp = serve(&big, true);
        assert!(udp.metadata.truncation && udp.answers.is_empty(), "a client without EDNS takes 512 bytes");
        let wide = serve(&query_bytes("big.example.org.", hickory_proto::rr::RecordType::TXT, Some(4096)), true);
        assert!(wide.metadata.truncation, "40 records of 200 bytes do not fit the capped 1232 either");
        let tcp = serve(&big, false);
        assert!(!tcp.metadata.truncation);
        assert_eq!(tcp.answers.len(), 40);
    }

    #[test]
    fn what_is_not_a_question_is_dropped_or_refused_with_a_code() {
        // Too short for an id, or an answer: no reply.
        assert_eq!(parse(&[]), Parsed::Drop);
        assert_eq!(parse(&[1]), Parsed::Drop);
        let mut answer_packet = query_bytes("example.org.", hickory_proto::rr::RecordType::A, None);
        answer_packet[2] |= 0x80;
        assert_eq!(parse(&answer_packet), Parsed::Drop);
        // Garbage with an id is a format error; a status request and an update are not implemented; another class is refused.
        assert_eq!(parse(&[0xab, 0xcd, 0x01, 0x00, 0xff, 0xff, 0, 0, 0, 0, 0, 0]), Parsed::Error(0xabcd, Error::FormErr));
        let mut status = query_bytes("example.org.", hickory_proto::rr::RecordType::A, None);
        status[2] = (status[2] & 0x87) | (2 << 3);
        assert_eq!(parse(&status), Parsed::Error(0x1234, Error::NotImp));
        let two = {
            let mut m = Message::query();
            m.metadata.id = 9;
            m.add_query(WireQuery::query(Name::from_ascii("a.example.org.").unwrap(), hickory_proto::rr::RecordType::A));
            m.add_query(WireQuery::query(Name::from_ascii("b.example.org.").unwrap(), hickory_proto::rr::RecordType::A));
            m.to_vec().unwrap()
        };
        assert_eq!(parse(&two), Parsed::Error(9, Error::FormErr), "one question at a time");
        let chaos = {
            let mut q = WireQuery::query(Name::from_ascii("version.bind.").unwrap(), hickory_proto::rr::RecordType::TXT);
            q.set_query_class(DNSClass::CH);
            let mut m = Message::query();
            m.metadata.id = 5;
            m.add_query(q);
            m.to_vec().unwrap()
        };
        assert_eq!(parse(&chaos), Parsed::Error(5, Error::Refused));
        let e = Message::from_vec(&encode_error(0xabcd, Error::FormErr)).unwrap();
        assert_eq!((e.metadata.id, e.metadata.response_code), (0xabcd, ResponseCode::FormErr));
    }

    #[test]
    fn an_edns_version_this_server_does_not_speak_is_badvers_and_the_payload_is_capped() {
        let mut m = Message::query();
        m.metadata.id = 3;
        m.add_query(WireQuery::query(Name::from_ascii("example.org.").unwrap(), hickory_proto::rr::RecordType::A));
        let mut e = Edns::new();
        e.set_version(1);
        m.set_edns(e);
        assert_eq!(parse(&m.to_vec().unwrap()), Parsed::Error(3, Error::BadVers));
        let Parsed::Query(q) = parse(&query_bytes("example.org.", hickory_proto::rr::RecordType::A, Some(65_000))) else { panic!() };
        assert_eq!(q.udp_payload, MAX_UDP_PAYLOAD);
        let Parsed::Query(small) = parse(&query_bytes("example.org.", hickory_proto::rr::RecordType::A, Some(100))) else { panic!() };
        assert_eq!(small.udp_payload, 512, "never below the classic size");
    }

    proptest::proptest! {
        /// No packet, however damaged, makes the parser panic; what comes back is one of the three outcomes.
        #[test]
        fn any_bytes_are_handled(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..300)) {
            let _ = parse(&bytes);
        }
    }
}

/// What the tests of the listeners need of the codec, so that the library is named here and nowhere else (public so that the integration tests can use it too).
pub mod testkit {
    use super::*;
    use hickory_proto::rr::RecordType;

    /// A question as bytes (`qtype` is the type number).
    pub fn question(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut m = Message::query();
        m.metadata.id = id;
        m.add_query(WireQuery::query(Name::from_ascii(name).unwrap(), RecordType::from(qtype)));
        m.to_vec().unwrap()
    }

    /// The parts of an answer the tests look at.
    #[derive(Debug)]
    pub struct Reply {
        pub id: u16,
        pub is_response: bool,
        pub authoritative: bool,
        pub truncated: bool,
        pub answers: usize,
        pub rcode: u8,
        pub size: usize,
    }

    pub fn reply(bytes: &[u8]) -> Reply {
        let m = Message::from_vec(bytes).unwrap();
        Reply {
            id: m.metadata.id,
            is_response: m.metadata.message_type == MessageType::Response,
            authoritative: m.metadata.authoritative,
            truncated: m.metadata.truncation,
            answers: m.answers.len(),
            rcode: u8::try_from(u16::from(m.metadata.response_code)).unwrap_or(255),
            size: bytes.len(),
        }
    }

    pub const FORMERR: u8 = 1;
    pub const REFUSED: u8 = 5;
}
