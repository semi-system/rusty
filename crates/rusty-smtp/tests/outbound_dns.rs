//! The production resolver path against a real (tiny) DNS server on loopback: `[outbound] nameserver`
//! must steer MX and address lookups, with the null MX and non-existent domain cases, and the TCP
//! dialer must honour `bind_address`. No test hooks: only the configuration keys a deployment uses.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use rusty_core::config::OutboundConfig;
use rusty_smtp::outbound::dial::{Dialer, TcpDialer};
use rusty_smtp::resolve::{MxAnswer, MxHost};
use rusty_smtp::stack::DnsStack;
use tokio::net::{TcpListener, UdpSocket};

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
    for l in n.split('.').filter(|l| !l.is_empty()) {
        v.push(l.len() as u8);
        v.extend_from_slice(l.as_bytes());
    }
    v.push(0);
    v
}

/// Answers MX for remote.test (10 mx.fake.test) and null.test (0 "."), A for mx.fake.test; NXDOMAIN otherwise.
async fn serve(sock: UdpSocket) {
    let mut buf = [0u8; 512];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
        let q = &buf[..n];
        let (name, end) = name_of(q);
        let qtype = u16::from_be_bytes([q[end], q[end + 1]]);
        let question = &q[12..end + 4];
        let mut answer: Vec<u8> = Vec::new();
        let mut rcode = 0u8;
        let mut count = 0u16;
        let mut rr = |rtype: u16, rdata: Vec<u8>| {
            answer.extend_from_slice(&[0xC0, 0x0C]);
            answer.extend_from_slice(&rtype.to_be_bytes());
            answer.extend_from_slice(&1u16.to_be_bytes());
            answer.extend_from_slice(&60u32.to_be_bytes());
            answer.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            answer.extend_from_slice(&rdata);
            count += 1;
        };
        match (name.as_str(), qtype) {
            ("remote.test", 15) => rr(15, [&10u16.to_be_bytes()[..], &encode_name("mx.fake.test")].concat()),
            ("null.test", 15) => rr(15, [&0u16.to_be_bytes()[..], &[0u8][..]].concat()),
            ("mx.fake.test", 1) => rr(1, vec![192, 0, 2, 10]),
            ("remote.test" | "null.test" | "mx.fake.test", _) => {} // exists, no data of this type
            _ => rcode = 3,
        }
        let mut resp = vec![q[0], q[1], 0x81, 0x80 | rcode, 0, 1, (count >> 8) as u8, count as u8, 0, 0, 0, 0];
        resp.extend_from_slice(question);
        resp.extend_from_slice(&answer);
        let _ = sock.send_to(&resp, from).await;
    }
}

// backend-neutral: resolver and dialer over loopback sockets, no database
#[tokio::test]
async fn the_nameserver_key_steers_mx_and_address_lookups() {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ns: SocketAddr = sock.local_addr().unwrap();
    let server = tokio::spawn(serve(sock));
    let cfg = OutboundConfig { nameserver: Some(ns), ..OutboundConfig::default() };
    let stack = DnsStack::from_config(&cfg, "mx.example.net").unwrap();
    let r = &stack.resolver;
    macro_rules! t {
        ($f:expr) => {
            tokio::time::timeout(Duration::from_secs(10), $f)
        };
    }
    assert_eq!(t!(r.mx("remote.test")).await.unwrap().unwrap(), MxAnswer::Hosts(vec![MxHost { preference: 10, host: "mx.fake.test".into() }]));
    assert_eq!(t!(r.addresses("mx.fake.test")).await.unwrap().unwrap(), vec!["192.0.2.10".parse::<IpAddr>().unwrap()]);
    assert_eq!(t!(r.mx("null.test")).await.unwrap().unwrap(), MxAnswer::NullMx, "RFC 7505");
    assert_eq!(t!(r.mx("nothere.test")).await.unwrap().unwrap(), MxAnswer::NoSuchDomain);
    assert!(t!(r.addresses("nothere.test")).await.unwrap().unwrap().is_empty());
    server.abort();
}

// backend-neutral: resolver and dialer over loopback sockets, no database
#[tokio::test]
async fn a_dead_nameserver_is_a_temporary_dns_failure_not_a_verdict() {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ns = sock.local_addr().unwrap();
    drop(sock); // nothing listens
    let cfg = OutboundConfig { nameserver: Some(ns), ..OutboundConfig::default() };
    let stack = DnsStack::from_config(&cfg, "mx.example.net").unwrap();
    let r = tokio::time::timeout(Duration::from_secs(30), stack.resolver.mx("remote.test")).await.unwrap();
    assert!(r.is_err(), "an unreachable resolver must defer the mail, never bounce it: {r:?}");
}

// backend-neutral: resolver and dialer over loopback sockets, no database
#[tokio::test]
async fn the_dialer_binds_the_configured_source_address() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let accept = tokio::spawn(async move { l.accept().await.unwrap().1.ip() });
    let bind: IpAddr = "127.0.0.1".parse().unwrap();
    let _s = TcpDialer.dial("127.0.0.1".parse().unwrap(), port, Some(bind), Duration::from_secs(5)).await.unwrap();
    assert_eq!(accept.await.unwrap(), bind);
    // A source address that is not ours cannot be bound: an error, not a silent fallback.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let foreign: IpAddr = "192.0.2.77".parse().unwrap();
    assert!(TcpDialer.dial("127.0.0.1".parse().unwrap(), port, Some(foreign), Duration::from_secs(2)).await.is_err());
}
