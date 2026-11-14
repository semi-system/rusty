//! `LiveDns`: the production `DnsSource` of the DNS validation engine, against a real (tiny) DNS server on loopback.
//! TXT records stay separate and their character-strings are joined, PTR and SRV come back as data, a name that does not
//! exist is an empty answer (not an error), and a failing server is a temporary failure. At the end the whole engine runs
//! over real DNS messages against a zone that publishes everything the generator asked for.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::OutboundConfig;
use rusty_core::dns::{records_for_domain, reverse_name, DkimKind, DkimPublic, DnsInputs};
use rusty_core::dns_check::{check_records, CheckOptions, DnsFail, DnsSource, MxEntry};
use rusty_core::dns_state::RecordStatus;
use rusty_smtp::stack::LiveDns;
use tokio::net::UdpSocket;

fn name_of(q: &[u8]) -> (String, usize) {
    let mut i = 12;
    let mut labels = Vec::new();
    while let Some(&n) = q.get(i) {
        if n == 0 {
            i += 1;
            break;
        }
        labels.push(String::from_utf8_lossy(q.get(i + 1..i + 1 + n as usize).unwrap()).to_ascii_lowercase());
        i += 1 + n as usize;
    }
    (labels.join("."), i)
}

fn encode_name(n: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for l in n.trim_end_matches('.').split('.').filter(|l| !l.is_empty()) {
        v.push(l.len() as u8);
        v.extend_from_slice(l.as_bytes());
    }
    v.push(0);
    v
}

fn txt(strings: &[&str]) -> Vec<u8> {
    strings.iter().flat_map(|s| std::iter::once(s.len() as u8).chain(s.bytes())).collect()
}

fn mx(pref: u16, host: &str) -> Vec<u8> {
    [&pref.to_be_bytes()[..], &encode_name(host)].concat()
}

fn srv(prio: u16, weight: u16, port: u16, target: &str) -> Vec<u8> {
    [&prio.to_be_bytes()[..], &weight.to_be_bytes(), &port.to_be_bytes(), &encode_name(target)].concat()
}

const MX: u16 = 15;
const A: u16 = 1;
const AAAA: u16 = 28;
const PTR: u16 = 12;
const TXT: u16 = 16;
const SRV: u16 = 33;

#[derive(Default)]
struct Zone {
    rr: HashMap<(String, u16), Vec<Vec<u8>>>,
    servfail: HashSet<String>,
}

impl Zone {
    fn add(&mut self, name: &str, rtype: u16, rdata: Vec<u8>) {
        self.rr.entry((name.trim_end_matches('.').to_ascii_lowercase(), rtype)).or_default().push(rdata);
    }
}

async fn serve(sock: UdpSocket, zone: Arc<Zone>) {
    let mut buf = [0u8; 512];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
        let q = &buf[..n];
        let (name, end) = name_of(q);
        let qtype = u16::from_be_bytes([q[end], q[end + 1]]);
        let question = &q[12..end + 4];
        let mut answer: Vec<u8> = Vec::new();
        let mut count = 0u16;
        let rcode = if zone.servfail.contains(&name) {
            2u8
        } else if let Some(list) = zone.rr.get(&(name.clone(), qtype)) {
            for rdata in list {
                answer.extend_from_slice(&[0xC0, 0x0C]);
                answer.extend_from_slice(&qtype.to_be_bytes());
                answer.extend_from_slice(&1u16.to_be_bytes());
                answer.extend_from_slice(&60u32.to_be_bytes());
                answer.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
                answer.extend_from_slice(rdata);
                count += 1;
            }
            0
        } else if zone.rr.keys().any(|(n, _)| *n == name) {
            0 // the name exists, there is no data of this type
        } else {
            3 // NXDOMAIN
        };
        let mut resp = vec![q[0], q[1], 0x81, 0x80 | rcode, 0, 1, (count >> 8) as u8, count as u8, 0, 0, 0, 0];
        resp.extend_from_slice(question);
        resp.extend_from_slice(&answer);
        let _ = sock.send_to(&resp, from).await;
    }
}

async fn live(zone: Zone) -> (LiveDns, tokio::task::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ns: SocketAddr = sock.local_addr().unwrap();
    let server = tokio::spawn(serve(sock, Arc::new(zone)));
    let cfg = OutboundConfig { nameserver: Some(ns), ..OutboundConfig::default() };
    (LiveDns::from_config(&cfg).unwrap(), server)
}

macro_rules! t {
    ($f:expr) => {
        tokio::time::timeout(Duration::from_secs(10), $f).await.unwrap()
    };
}

#[tokio::test]
async fn txt_records_stay_separate_and_their_strings_are_joined() {
    let mut z = Zone::default();
    z.add("multi.test", TXT, txt(&["v=spf1 -all"]));
    z.add("multi.test", TXT, txt(&["abc", "def", "ghi"]));
    z.add("empty-type.test", A, vec![192, 0, 2, 1]);
    let (dns, server) = live(z).await;
    let mut got = t!(dns.txt("multi.test")).unwrap();
    got.sort();
    assert_eq!(got, ["abcdefghi", "v=spf1 -all"]);
    assert_eq!(t!(dns.txt("empty-type.test")).unwrap(), Vec::<String>::new(), "the name exists, no TXT data");
    assert_eq!(t!(dns.txt("nothere.test")).unwrap(), Vec::<String>::new(), "NXDOMAIN is an empty answer, not an error");
    server.abort();
}

#[tokio::test]
async fn mx_addresses_ptr_and_srv() {
    let mut z = Zone::default();
    z.add("example.test", MX, mx(10, "mx.example.test"));
    z.add("example.test", MX, mx(20, "Backup.Example.Test"));
    z.add("null.test", MX, mx(0, "."));
    z.add("mx.example.test", A, vec![192, 0, 2, 10]);
    let v6: IpAddr = "2001:db8::10".parse().unwrap();
    if let IpAddr::V6(a) = v6 {
        z.add("mx.example.test", AAAA, a.octets().to_vec());
    }
    z.add(&reverse_name("192.0.2.10".parse().unwrap()), PTR, encode_name("mx.example.test"));
    z.add(&reverse_name(v6), PTR, encode_name("mx.example.test"));
    z.add("_imaps._tcp.example.test", SRV, srv(0, 1, 993, "mx.example.test"));
    let (dns, server) = live(z).await;

    let mut hosts = t!(dns.mx("example.test")).unwrap();
    hosts.sort_by_key(|m| m.preference);
    assert_eq!(hosts, vec![MxEntry { preference: 10, host: "mx.example.test".into() }, MxEntry { preference: 20, host: "backup.example.test".into() }]);
    assert!(t!(dns.mx("null.test")).unwrap().is_empty(), "a null MX names no exchanger to compare");
    assert!(t!(dns.mx("nothere.test")).unwrap().is_empty());

    let mut addrs = t!(dns.addresses("mx.example.test")).unwrap();
    addrs.sort();
    assert_eq!(addrs, vec!["192.0.2.10".parse::<IpAddr>().unwrap(), v6]);
    assert!(t!(dns.addresses("nothere.test")).unwrap().is_empty());

    assert_eq!(t!(dns.ptr("192.0.2.10".parse().unwrap())).unwrap(), ["mx.example.test"]);
    assert_eq!(t!(dns.ptr(v6)).unwrap(), ["mx.example.test"], "the ip6.arpa form works too");
    assert!(t!(dns.ptr("192.0.2.99".parse().unwrap())).unwrap().is_empty());

    let s = t!(dns.srv("_imaps._tcp.example.test")).unwrap();
    assert_eq!((s[0].priority, s[0].weight, s[0].port, s[0].target.as_str()), (0, 1, 993, "mx.example.test"));
    assert!(t!(dns.srv("_imap._tcp.example.test")).unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn a_failing_server_is_a_temporary_failure_never_an_empty_answer() {
    let mut z = Zone::default();
    z.servfail.insert("broken.test".into());
    let (dns, server) = live(z).await;
    assert_eq!(t!(dns.txt("broken.test")), Err(DnsFail::Temporary));
    assert_eq!(t!(dns.mx("broken.test")), Err(DnsFail::Temporary));
    assert_eq!(t!(dns.addresses("broken.test")), Err(DnsFail::Temporary));
    assert_eq!(t!(dns.srv("broken.test")), Err(DnsFail::Temporary));
    server.abort();

    // Nothing listening at all.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ns = sock.local_addr().unwrap();
    drop(sock);
    let dead = LiveDns::from_config(&OutboundConfig { nameserver: Some(ns), ..OutboundConfig::default() }).unwrap();
    let r = tokio::time::timeout(Duration::from_secs(40), dead.txt("example.test")).await.unwrap();
    assert_eq!(r, Err(DnsFail::Temporary));
}

#[tokio::test]
async fn names_that_cannot_be_dns_names_are_an_empty_answer_not_a_crash() {
    let (dns, server) = live(Zone::default()).await;
    for bad in ["", "a..b", &"a".repeat(300), "bad name", "\u{1b}[31m"] {
        let r = t!(dns.txt(bad));
        assert!(matches!(r, Ok(ref v) if v.is_empty()), "{bad:.20?} -> {r:?}");
    }
    server.abort();
}

#[tokio::test]
async fn the_engine_over_real_dns_messages_finds_a_correct_publication_all_ok() {
    let ip: IpAddr = "192.0.2.10".parse().unwrap();
    let key = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxIDAQAB";
    let ips = [ip];
    let keys = [DkimPublic { selector: "rusty", kind: DkimKind::Rsa, public_key_b64: key }];
    let expected = records_for_domain(&DnsInputs { domain: "example.test", mail_host: "mx.example.test", outbound_ips: &ips, dkim: &keys, spf_includes: &[], dmarc_rua: Some("dmarc@example.test") }).unwrap();
    let mut z = Zone::default();
    z.add("example.test", MX, mx(10, "mx.example.test"));
    z.add("mx.example.test", A, vec![192, 0, 2, 10]);
    z.add("example.test", TXT, txt(&["v=spf1 mx ip4:192.0.2.10 -all"]));
    // A TXT longer than 255 bytes is several character-strings: the DKIM key.
    let dkim = format!("v=DKIM1; k=rsa; p={key}");
    let chunks: Vec<&str> = dkim.as_bytes().chunks(200).map(|c| std::str::from_utf8(c).unwrap()).collect();
    z.add("rusty._domainkey.example.test", TXT, txt(&chunks));
    z.add("_dmarc.example.test", TXT, txt(&["v=DMARC1; p=none; rua=mailto:dmarc@example.test"]));
    z.add(&reverse_name(ip), PTR, encode_name("mx.example.test"));
    for (s, port) in [("_submission._tcp", 587u16), ("_submissions._tcp", 465), ("_imaps._tcp", 993)] {
        z.add(&format!("{s}.example.test"), SRV, srv(0, 1, port, "mx.example.test"));
    }
    let (dns, server) = live(z).await;
    let checks = tokio::time::timeout(Duration::from_secs(30), check_records(&dns, &expected, &CheckOptions::default())).await.unwrap();
    for c in &checks {
        assert_eq!(c.status, RecordStatus::Ok, "{} {:?} -> {:?} {:?} seen {:?}", c.record.name, c.record.rtype, c.status, c.findings, c.seen);
    }
    assert_eq!(checks.len(), expected.len());
    server.abort();
}
