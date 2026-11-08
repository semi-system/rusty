//! The listener loop: graceful shutdown with a drain deadline, connection limits, panic
//! isolation per session, the `Accept` seam (plain TCP and implicit TLS), the service runner.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::*;
use rusty_svc::{
    Accept, BoundListener, BoxedStream, DirectTcp, DirectTls, ListenerSpec, PeerInfo, ServiceEvent,
    ServiceEvents, SessionHandler, TlsProvider,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);
impl ServiceEvents for Recorder {
    fn emit(&self, ev: ServiceEvent<'_>) {
        self.0.lock().unwrap().push(format!("{ev:?}"));
    }
}
impl Recorder {
    fn count(&self, needle: &str) -> usize {
        self.0.lock().unwrap().iter().filter(|e| e.contains(needle)).count()
    }
}

/// Reads until the client closes or shutdown is requested, then says goodbye (a drain).
struct Drainer {
    seen_tls: Mutex<Vec<bool>>,
}
#[async_trait]
impl SessionHandler for Drainer {
    async fn handle(&self, mut stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        self.seen_tls.lock().unwrap().push(peer.tls);
        let mut buf = [0u8; 16];
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = stream.read(&mut buf) => {}
        }
        let _ = stream.write_all(b"bye\n").await;
        let _ = stream.shutdown().await;
    }
}

/// Ignores shutdown entirely (a stuck session): the deadline must cut it.
struct Stuck;
#[async_trait]
impl SessionHandler for Stuck {
    async fn handle(&self, _s: BoxedStream, _p: PeerInfo, _sd: CancellationToken) {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// Panics on the first connection, serves the rest.
struct Panicky(Mutex<u32>);
#[async_trait]
impl SessionHandler for Panicky {
    async fn handle(&self, mut s: BoxedStream, _p: PeerInfo, _sd: CancellationToken) {
        let first = {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            *n == 1
        };
        if first {
            panic!("session bug");
        }
        let _ = s.write_all(b"ok\n").await;
    }
}

fn spec(max: u32, per_ip: u32) -> ListenerSpec {
    ListenerSpec {
        name: "test",
        addr: "127.0.0.1:0".parse().unwrap(),
        max_connections: max,
        max_per_ip: per_ip,
    }
}

struct Running {
    addr: SocketAddr,
    token: CancellationToken,
    join: tokio::task::JoinHandle<rusty_core::Result<()>>,
    events: Arc<Recorder>,
}

async fn start(
    spec: ListenerSpec,
    accept: Arc<dyn Accept>,
    handler: Arc<dyn SessionHandler>,
    grace: Duration,
) -> Running {
    let l = BoundListener::bind(spec).await.unwrap();
    let addr = l.local_addr();
    let token = CancellationToken::new();
    let events = Arc::new(Recorder::default());
    let join = tokio::spawn(l.serve(accept, handler, events.clone(), token.clone(), grace));
    Running { addr, token, join, events }
}

async fn read_all(s: &mut TcpStream) -> String {
    let mut out = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_string(&mut out)).await;
    out
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn shutdown_closes_the_listener_and_drains_an_idle_session_within_the_deadline() {
    let h = Arc::new(Drainer { seen_tls: Mutex::new(vec![]) });
    let r = start(spec(10, 10), Arc::new(DirectTcp), h.clone(), Duration::from_secs(5)).await;
    let mut idle = TcpStream::connect(r.addr).await.unwrap();
    // wait until the session task has started
    while h.seen_tls.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let t0 = Instant::now();
    r.token.cancel();
    // the idle session is drained, it was not cut: it still got to say goodbye
    assert_eq!(read_all(&mut idle).await, "bye\n");
    tokio::time::timeout(Duration::from_secs(5), r.join).await.unwrap().unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(4), "took {:?}", t0.elapsed());
    // and the listener is closed
    assert!(TcpStream::connect(r.addr).await.is_err(), "listener still accepting after shutdown");
    assert_eq!(*h.seen_tls.lock().unwrap(), vec![false]);
    assert_eq!(r.events.count("Closed"), 1);
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn a_session_that_ignores_shutdown_is_cut_at_the_deadline() {
    let r = start(spec(10, 10), Arc::new(DirectTcp), Arc::new(Stuck), Duration::from_millis(300)).await;
    let mut c = TcpStream::connect(r.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    r.token.cancel();
    tokio::time::timeout(Duration::from_secs(5), r.join).await.unwrap().unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(3), "took {:?}", t0.elapsed());
    assert_eq!(read_all(&mut c).await, "", "the stuck session's socket is closed");
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn a_panicking_session_drops_only_itself() {
    let r = start(spec(10, 10), Arc::new(DirectTcp), Arc::new(Panicky(Mutex::new(0))), Duration::from_secs(2)).await;
    let mut first = TcpStream::connect(r.addr).await.unwrap();
    assert_eq!(read_all(&mut first).await, "", "the panicked session just closes");
    let mut second = TcpStream::connect(r.addr).await.unwrap();
    assert_eq!(read_all(&mut second).await, "ok\n", "the listener keeps serving");
    assert_eq!(r.events.count("SessionPanicked"), 1);
    r.token.cancel();
    r.join.await.unwrap().unwrap();
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn connection_limits_total_and_per_ip() {
    let h = Arc::new(Drainer { seen_tls: Mutex::new(vec![]) });
    // per-IP 2 (all test clients are 127.0.0.1), total 100
    let r = start(spec(100, 2), Arc::new(DirectTcp), h.clone(), Duration::from_secs(2)).await;
    let c1 = TcpStream::connect(r.addr).await.unwrap();
    let c2 = TcpStream::connect(r.addr).await.unwrap();
    while h.seen_tls.lock().unwrap().len() < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut c3 = TcpStream::connect(r.addr).await.unwrap();
    assert_eq!(read_all(&mut c3).await, "", "third connection from one IP is refused");
    assert_eq!(r.events.count("PerIpLimit"), 1);
    drop(c1);
    // a slot frees up once the first session ends
    let mut c4 = loop {
        let mut c = TcpStream::connect(r.addr).await.unwrap();
        let mut b = [0u8; 1];
        // an accepted connection stays open (the Drainer waits for input); a refused one is closed
        if tokio::time::timeout(Duration::from_millis(200), c.read(&mut b)).await.is_err() {
            break c;
        }
    };
    let _ = c4.write_all(b"x").await;
    drop(c2);
    r.token.cancel();
    r.join.await.unwrap().unwrap();

    // total cap 1
    let r = start(spec(1, 50), Arc::new(DirectTcp), Arc::new(Drainer { seen_tls: Mutex::new(vec![]) }), Duration::from_secs(2)).await;
    let _a = TcpStream::connect(r.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut b = TcpStream::connect(r.addr).await.unwrap();
    assert_eq!(read_all(&mut b).await, "");
    assert_eq!(r.events.count("TotalLimit"), 1);
    r.token.cancel();
    r.join.await.unwrap().unwrap();
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn implicit_tls_accepter_hands_the_session_a_tls_stream_and_peer_info() {
    let d = tempfile::tempdir().unwrap();
    let (cert, key) = make_pems(&["mail.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert.as_bytes(), 0o644);
    let k = write_file(d.path(), "key.pem", key.as_bytes(), 0o600);
    let tls_cfg = rusty_core::config::TlsConfig { cert_file: Some(c), key_file: Some(k), dev_self_signed: false };
    let provider =
        TlsProvider::from_config(&tls_cfg, "mail.example.test", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    let h = Arc::new(Drainer { seen_tls: Mutex::new(vec![]) });
    let r = start(spec(10, 10), Arc::new(DirectTls::new(provider)), h.clone(), Duration::from_secs(2)).await;
    let (mut s, _) = tls_connect(r.addr, "mail.example.test", &cert).await;
    s.write_all(b"hello").await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap_or(0);
    assert_eq!(out, "bye\n");
    assert_eq!(*h.seen_tls.lock().unwrap(), vec![true]);
    r.token.cancel();
    r.join.await.unwrap().unwrap();
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn a_client_that_never_finishes_the_tls_handshake_is_dropped() {
    let d = tempfile::tempdir().unwrap();
    let (cert, key) = make_pems(&["mail.example.test"]);
    let c = write_file(d.path(), "cert.pem", cert.as_bytes(), 0o644);
    let k = write_file(d.path(), "key.pem", key.as_bytes(), 0o600);
    let tls_cfg = rusty_core::config::TlsConfig { cert_file: Some(c), key_file: Some(k), dev_self_signed: false };
    let provider =
        TlsProvider::from_config(&tls_cfg, "mail.example.test", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    let accept = Arc::new(DirectTls::new(provider).with_handshake_timeout(Duration::from_millis(200)));
    let h = Arc::new(Drainer { seen_tls: Mutex::new(vec![]) });
    let r = start(spec(10, 10), accept, h.clone(), Duration::from_secs(2)).await;
    let mut slow = TcpStream::connect(r.addr).await.unwrap();
    // send nothing: the server must hang up on its own
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), slow.read(&mut b)).await.expect("server kept the socket open");
    assert!(matches!(n, Ok(0) | Err(_)));
    assert!(h.seen_tls.lock().unwrap().is_empty(), "no session may start without a completed handshake");
    r.token.cancel();
    r.join.await.unwrap().unwrap();
}

// backend-neutral: no database involved, socket loop only
#[tokio::test]
async fn binding_a_busy_port_is_an_error() {
    let l = BoundListener::bind(spec(1, 1)).await.unwrap();
    let mut s2 = spec(1, 1);
    s2.addr = l.local_addr();
    assert!(BoundListener::bind(s2).await.is_err());
}
