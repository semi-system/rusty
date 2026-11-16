//! The PROXY protocol v2 parser (every refusal) and the edge accepter on a real listener: a header from a trusted proxy names the client, a header from anyone
//! else is never read, garbage and slowness are dropped without hurting the service, and the per-address cap counts the real client.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::cidr::Cidr;
use rusty_svc::proxy::{encode, parse, payload_len, EdgeAccept, Header, ProxyError, MAX_PAYLOAD, PREFIX_LEN, SIGNATURE};
use rusty_svc::{BoundListener, BoxedStream, ListenerSpec, PeerInfo, SessionHandler, TracingEvents};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn split(h: &[u8]) -> ([u8; PREFIX_LEN], Vec<u8>) {
    let mut p = [0u8; PREFIX_LEN];
    p.copy_from_slice(&h[..PREFIX_LEN]);
    (p, h[PREFIX_LEN..].to_vec())
}

fn parsed(h: &[u8]) -> Result<Header, ProxyError> {
    let (p, payload) = split(h);
    parse(&p, &payload)
}

// ------------------------------------------------------------------------------------------------ the parser

#[test]
fn a_v4_and_a_v6_header_give_the_client_address() {
    let h = parsed(&encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), false)).unwrap();
    assert_eq!((h.source, h.ssl), (Some(sa("203.0.113.9:4000")), false));
    let h = parsed(&encode(sa("[2001:db8::7]:5000"), sa("[2001:db8::1]:25"), false)).unwrap();
    assert_eq!(h.source, Some(sa("[2001:db8::7]:5000")));
}

#[test]
fn the_ssl_tlv_says_the_client_used_tls_at_the_edge() {
    assert!(parsed(&encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), true)).unwrap().ssl);
    // A TLV whose client flag is clear is not TLS.
    let mut h = encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), true);
    let at = h.len() - 5;
    h[at] = 0;
    assert!(!parsed(&h).unwrap().ssl);
}

#[test]
fn local_and_an_unspecified_family_keep_the_proxys_own_address() {
    let mut local = SIGNATURE.to_vec();
    local.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
    assert_eq!(parsed(&local).unwrap(), Header { source: None, ssl: false });
    // LOCAL may carry bytes (TLVs); they are ignored.
    let mut local_tlv = SIGNATURE.to_vec();
    local_tlv.extend_from_slice(&[0x20, 0x00, 0x00, 0x03, 0xee, 0, 0]);
    assert_eq!(parsed(&local_tlv).unwrap().source, None);
    let mut unspec = SIGNATURE.to_vec();
    unspec.extend_from_slice(&[0x21, 0x00, 0x00, 0x00]);
    assert_eq!(parsed(&unspec).unwrap().source, None);
}

#[test]
fn every_malformed_header_is_refused_with_a_fixed_reason() {
    let good = encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), false);
    let with = |at: usize, v: u8| {
        let mut h = good.clone();
        h[at] = v;
        h
    };
    assert_eq!(payload_len(&split(&with(0, b'X')).0), Err(ProxyError::Signature));
    assert_eq!(payload_len(&split(&with(11, b'X')).0), Err(ProxyError::Signature));
    assert_eq!(payload_len(&split(&with(12, 0x11)).0), Err(ProxyError::Version), "version 1 in the version nibble");
    assert_eq!(payload_len(&split(&with(12, 0x31)).0), Err(ProxyError::Version));
    assert_eq!(payload_len(&split(&with(12, 0x22)).0), Err(ProxyError::Command));
    assert_eq!(parsed(&with(13, 0x12)), Err(ProxyError::Transport), "datagram");
    assert_eq!(parsed(&with(13, 0x30)), Err(ProxyError::Transport).or(Err(ProxyError::Family)), "UNIX");
    assert_eq!(parsed(&with(13, 0x31)), Err(ProxyError::Family), "UNIX stream");
    assert_eq!(parsed(&with(13, 0xf1)), Err(ProxyError::Family));
    // The text protocol of version 1 is not accepted.
    let v1 = b"PROXY TCP4 203.0.113.9 192.0.2.1 4000 993\r\n";
    let mut p = [0u8; PREFIX_LEN];
    p.copy_from_slice(&v1[..PREFIX_LEN]);
    assert_eq!(payload_len(&p), Err(ProxyError::Signature));
    // A payload shorter than its address block, and a length over the cap.
    let (p, payload) = split(&good);
    assert_eq!(parse(&p, &payload[..payload.len() - 1]), Err(ProxyError::Truncated));
    assert_eq!(parse(&p, &[payload.clone(), vec![0]].concat()), Err(ProxyError::Truncated), "more than announced");
    let mut big = good.clone();
    big[14..16].copy_from_slice(&u16::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes());
    assert_eq!(payload_len(&split(&big).0), Err(ProxyError::TooLong));
    let mut max = good.clone();
    max[14..16].copy_from_slice(&u16::try_from(MAX_PAYLOAD).unwrap().to_be_bytes());
    assert_eq!(payload_len(&split(&max).0), Ok(MAX_PAYLOAD));
    // An address block cut short by its own announced length.
    let mut short = SIGNATURE.to_vec();
    short.extend_from_slice(&[0x21, 0x11, 0x00, 0x05, 1, 2, 3, 4, 5]);
    assert_eq!(parsed(&short), Err(ProxyError::Truncated));
}

#[test]
fn a_tlv_that_overruns_the_payload_or_is_cut_is_refused() {
    let base = |tlvs: &[u8]| {
        let mut h = SIGNATURE.to_vec();
        h.extend_from_slice(&[0x21, 0x11]);
        h.extend_from_slice(&u16::try_from(12 + tlvs.len()).unwrap().to_be_bytes());
        h.extend_from_slice(&[203, 0, 113, 9, 192, 0, 2, 1, 0x0f, 0xa0, 0x03, 0xe1]);
        h.extend_from_slice(tlvs);
        h
    };
    assert!(parsed(&base(&[0x04, 0x00, 0x02, 0xaa, 0xbb])).is_ok(), "an unknown TLV is skipped");
    for bad in [&[0x04][..], &[0x04, 0x00], &[0x04, 0x00, 0x05, 1, 2], &[0x20, 0x00, 0x02, 0x01, 0x00], &[0x20, 0x00, 0x00], &[0x04, 0xff, 0xff]] {
        assert_eq!(parsed(&base(bad)), Err(ProxyError::Tlv), "{bad:?}");
    }
}

proptest::proptest! {
    #[test]
    fn whatever_the_bytes_the_parser_returns_without_panicking(prefix in proptest::collection::vec(proptest::prelude::any::<u8>(), 16), payload in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300)) {
        let mut p = [0u8; PREFIX_LEN];
        p.copy_from_slice(&prefix);
        let _ = payload_len(&p);
        let _ = parse(&p, &payload);
        // A valid signature with arbitrary rest.
        p[..12].copy_from_slice(&SIGNATURE);
        let _ = parse(&p, &payload);
    }

    #[test]
    fn what_encode_makes_parses_back(a in proptest::prelude::any::<[u8; 4]>(), b in proptest::prelude::any::<[u8; 16]>(), pa in proptest::prelude::any::<u16>(), pb in proptest::prelude::any::<u16>(), ssl in proptest::prelude::any::<bool>(), v6 in proptest::prelude::any::<bool>()) {
        let (s, d) = if v6 { (SocketAddr::new(IpAddr::from(b), pa), SocketAddr::new(IpAddr::from(b), pb)) } else { (SocketAddr::new(IpAddr::from(a), pa), SocketAddr::new(IpAddr::from(a), pb)) };
        let h = parsed(&encode(s, d, ssl)).unwrap();
        proptest::prop_assert_eq!(h.source, Some(s));
        proptest::prop_assert_eq!(h.ssl, ssl);
    }
}

// ------------------------------------------------------------------------------------------------ the accepter on a real listener

/// Reports each session: the peer it was given and the first line the client sent after the header.
struct Probe {
    seen: mpsc::UnboundedSender<(PeerInfo, String)>,
    live: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl SessionHandler for Probe {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        self.live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut r = BufReader::new(stream);
        let mut line = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut line)).await;
        let _ = self.seen.send((peer, line.trim_end().to_owned()));
        // Stay open until the client goes away (so the per-address slot stays taken).
        let mut rest = String::new();
        tokio::select! {
            _ = r.read_line(&mut rest) => {}
            () = shutdown.cancelled() => {}
        }
        self.live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

struct Edge {
    addr: SocketAddr,
    seen: mpsc::UnboundedReceiver<(PeerInfo, String)>,
    live: Arc<std::sync::atomic::AtomicUsize>,
    stop: CancellationToken,
}

impl Drop for Edge {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn edge(trusted: &[&str], edge_tls: bool, timeout: Duration, max_per_ip: u32) -> Edge {
    let trusted: Vec<Cidr> = trusted.iter().map(|c| c.parse().unwrap()).collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let spec = ListenerSpec { name: "edge-test", addr: sa("127.0.0.1:0"), max_connections: 64, max_per_ip };
    let bound = BoundListener::bind(spec).await.unwrap();
    let addr = bound.local_addr();
    let stop = CancellationToken::new();
    let s = stop.clone();
    let accept = Arc::new(EdgeAccept::new(trusted, edge_tls).with_header_timeout(timeout));
    let handler = Arc::new(Probe { seen: tx, live: live.clone() });
    tokio::spawn(async move {
        let _ = bound.serve(accept, handler, Arc::new(TracingEvents), s, Duration::from_secs(2)).await;
    });
    Edge { addr, seen: rx, live, stop }
}

async fn next(e: &mut Edge) -> Option<(PeerInfo, String)> {
    tokio::time::timeout(Duration::from_secs(3), e.seen.recv()).await.ok().flatten()
}

async fn nothing_arrives(e: &mut Edge) -> bool {
    tokio::time::timeout(Duration::from_millis(600), e.seen.recv()).await.is_err()
}

/// Is the connection closed by the server (EOF or reset) within a moment?
async fn closed(mut c: TcpStream) -> bool {
    use tokio::io::AsyncReadExt;
    let mut b = [0u8; 16];
    matches!(tokio::time::timeout(Duration::from_secs(3), c.read(&mut b)).await, Ok(Ok(0)) | Ok(Err(_)))
}

#[tokio::test]
async fn a_trusted_proxy_names_the_client_and_the_bytes_after_the_header_reach_the_session() {
    let mut e = edge(&["127.0.0.0/8"], false, Duration::from_secs(2), 8).await;
    let mut c = TcpStream::connect(e.addr).await.unwrap();
    // Header and the first command in ONE write: nothing after the header may be swallowed.
    let mut data = encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), false);
    data.extend_from_slice(b"hello\r\n");
    c.write_all(&data).await.unwrap();
    let (peer, line) = next(&mut e).await.expect("a session");
    assert_eq!((peer.addr, peer.tls, line.as_str()), (sa("203.0.113.9:4000"), false, "hello"));
    // With the SSL TLV the connection counts as protected.
    let mut c2 = TcpStream::connect(e.addr).await.unwrap();
    c2.write_all(&[encode(sa("203.0.113.10:4001"), sa("192.0.2.1:993"), true), b"x\r\n".to_vec()].concat()).await.unwrap();
    let (peer, _) = next(&mut e).await.unwrap();
    assert_eq!((peer.addr, peer.tls), (sa("203.0.113.10:4001"), true));
}

#[tokio::test]
async fn an_implicit_tls_port_behind_the_edge_is_already_protected() {
    let mut e = edge(&["127.0.0.0/8"], true, Duration::from_secs(2), 8).await;
    let mut c = TcpStream::connect(e.addr).await.unwrap();
    c.write_all(&[encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), false), b"x\r\n".to_vec()].concat()).await.unwrap();
    let (peer, _) = next(&mut e).await.unwrap();
    assert!(peer.tls, "the edge terminated TLS on this port");
}

#[tokio::test]
async fn a_proxys_health_check_keeps_the_proxys_own_address() {
    let mut e = edge(&["127.0.0.0/8"], false, Duration::from_secs(2), 8).await;
    let mut c = TcpStream::connect(e.addr).await.unwrap();
    let local = c.local_addr().unwrap();
    let mut h = SIGNATURE.to_vec();
    h.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
    h.extend_from_slice(b"ping\r\n");
    c.write_all(&h).await.unwrap();
    let (peer, line) = next(&mut e).await.unwrap();
    assert_eq!((peer.addr, line.as_str()), (local, "ping"));
}

#[tokio::test]
async fn a_header_from_an_address_that_is_not_a_trusted_proxy_is_never_read() {
    // Only 10.0.0.0/8 is trusted; the client connects from 127.0.0.1 and presents a perfect header claiming another address.
    let mut e = edge(&["10.0.0.0/8"], false, Duration::from_secs(2), 8).await;
    let mut c = TcpStream::connect(e.addr).await.unwrap();
    let _ = c.write_all(&[encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), true), b"hello\r\n".to_vec()].concat()).await;
    assert!(closed(c).await, "the connection is dropped");
    assert!(nothing_arrives(&mut e).await, "no session was ever started, so no claimed address was believed");
}

#[tokio::test]
async fn garbage_slowness_and_oversized_headers_are_dropped_and_the_service_keeps_working() {
    let mut e = edge(&["127.0.0.0/8"], false, Duration::from_millis(300), 8).await;
    let v1 = b"PROXY TCP4 203.0.113.9 192.0.2.1 4000 993\r\nhello\r\n".to_vec();
    let mut oversize = SIGNATURE.to_vec();
    oversize.extend_from_slice(&[0x21, 0x11, 0xff, 0xff]);
    oversize.extend_from_slice(&[0u8; 64]);
    let mut udp = encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), false);
    udp[13] = 0x12;
    for (what, bytes) in [("no header at all", b"EHLO example\r\n".to_vec()), ("version 1 text", v1), ("a length over the cap", oversize), ("a datagram transport", udp), ("a short header", SIGNATURE[..9].to_vec())] {
        let mut c = TcpStream::connect(e.addr).await.unwrap();
        let _ = c.write_all(&bytes).await;
        assert!(closed(c).await, "{what}");
    }
    // A proxy that connects and says nothing is cut when the header timeout ends.
    let silent = TcpStream::connect(e.addr).await.unwrap();
    assert!(closed(silent).await, "slow header");
    assert!(nothing_arrives(&mut e).await, "none of those became a session");
    // And a good one still works afterwards.
    let mut c = TcpStream::connect(e.addr).await.unwrap();
    c.write_all(&[encode(sa("203.0.113.9:4000"), sa("192.0.2.1:993"), false), b"ok\r\n".to_vec()].concat()).await.unwrap();
    let (peer, line) = next(&mut e).await.expect("the service still serves");
    assert_eq!((peer.addr.ip().to_string().as_str(), line.as_str()), ("203.0.113.9", "ok"));
}

#[tokio::test]
async fn the_per_address_cap_counts_the_real_client_not_the_proxy() {
    // All connections come from 127.0.0.1 (the "proxy"); the cap is 1 per client address.
    let mut e = edge(&["127.0.0.0/8"], false, Duration::from_secs(2), 1).await;
    let hello = |client: &str| [encode(sa(client), sa("192.0.2.1:993"), false), b"x\r\n".to_vec()].concat();
    let mut a1 = TcpStream::connect(e.addr).await.unwrap();
    a1.write_all(&hello("203.0.113.9:4000")).await.unwrap();
    assert!(next(&mut e).await.is_some());
    // The same client again: refused (the slot is taken), the connection is closed.
    let mut a2 = TcpStream::connect(e.addr).await.unwrap();
    let _ = a2.write_all(&hello("203.0.113.9:4001")).await;
    assert!(closed(a2).await, "a second connection of the same client");
    // Another client behind the same proxy is not affected.
    let mut b1 = TcpStream::connect(e.addr).await.unwrap();
    b1.write_all(&hello("203.0.113.77:4000")).await.unwrap();
    let (peer, _) = next(&mut e).await.expect("another client");
    assert_eq!(peer.addr.ip().to_string(), "203.0.113.77");
    assert_eq!(e.live.load(std::sync::atomic::Ordering::SeqCst), 2);
    // When the first client leaves, its slot is free again.
    drop(a1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut a3 = TcpStream::connect(e.addr).await.unwrap();
    a3.write_all(&hello("203.0.113.9:4002")).await.unwrap();
    assert!(next(&mut e).await.is_some());
}

#[test]
fn the_fuzz_seeds_hold_the_invariants() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/fuzz_proxy_v2");
    let mut n = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        rusty_svc::proxy::check_invariants(&std::fs::read(entry.unwrap().path()).unwrap());
        n += 1;
    }
    assert!(n >= 4);
}

proptest::proptest! {
    #[test]
    fn the_invariants_hold_for_arbitrary_bytes(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..400)) {
        rusty_svc::proxy::check_invariants(&data);
    }
}

/// An edge that trusts the addresses of a name (no block at all).
async fn edge_named(names: Vec<String>) -> Edge {
    let (tx, rx) = mpsc::unbounded_channel();
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let spec = ListenerSpec { name: "edge-named", addr: sa("127.0.0.1:0"), max_connections: 64, max_per_ip: 8 };
    let bound = BoundListener::bind(spec).await.unwrap();
    let addr = bound.local_addr();
    let stop = CancellationToken::new();
    let s = stop.clone();
    let named = rusty_svc::NamedPeers::start(names, Duration::from_secs(60), stop.clone()).await;
    let accept = Arc::new(EdgeAccept::new(Vec::new(), false).with_named(named));
    let handler = Arc::new(Probe { seen: tx, live: live.clone() });
    tokio::spawn(async move {
        let _ = bound.serve(accept, handler, Arc::new(TracingEvents), s, Duration::from_secs(2)).await;
    });
    Edge { addr, seen: rx, live, stop }
}

#[tokio::test]
async fn the_edge_may_be_trusted_by_its_name_and_a_name_that_is_not_the_peer_is_not_trusted() {
    // `localhost` is where the test connects from: its header is believed.
    let mut e = edge_named(vec!["localhost".into()]).await;
    let mut c = TcpStream::connect(e.addr).await.unwrap();
    c.write_all(&[encode(sa("203.0.113.9:4000"), sa("192.0.2.1:25"), false), b"hello\r\n".to_vec()].concat()).await.unwrap();
    let (peer, line) = next(&mut e).await.expect("a session");
    assert_eq!((peer.addr, line.as_str()), (sa("203.0.113.9:4000"), "hello"));
    // A name that does not resolve trusts nobody: the header is never read.
    let mut e2 = edge_named(vec!["no-such-service.invalid".into()]).await;
    let mut c2 = TcpStream::connect(e2.addr).await.unwrap();
    let _ = c2.write_all(&[encode(sa("203.0.113.9:4000"), sa("192.0.2.1:25"), false), b"hello\r\n".to_vec()].concat()).await;
    assert!(closed(c2).await);
    assert!(nothing_arrives(&mut e2).await);
}
