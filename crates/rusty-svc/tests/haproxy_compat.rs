//! The edge accepter against a REAL HAProxy (the image pinned in deploy/images.lock): the header HAProxy's `send-proxy-v2` produces is parsed, the client
//! address in it is the peer, its health checks (`check-send-proxy`, the `LOCAL` command) do not break the accepter, and the client behind HAProxy is the
//! peer the session sees, not HAProxy.
//!
//! Needs Docker with host networking (Linux/WSL), so it is `#[ignore]`d in the per-commit run; the nightly edge job runs it:
//!   cargo test -p rusty-svc --test haproxy_compat -- --ignored --nocapture

use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_svc::proxy::EdgeAccept;
use rusty_svc::{BoundListener, BoxedStream, ListenerSpec, PeerInfo, SessionHandler, TracingEvents};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Probe(mpsc::UnboundedSender<(PeerInfo, String)>);

#[async_trait]
impl SessionHandler for Probe {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, _shutdown: CancellationToken) {
        let mut r = BufReader::new(stream);
        let mut line = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), r.read_line(&mut line)).await;
        let _ = self.0.send((peer, line.trim_end().to_owned()));
    }
}

fn image() -> String {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/images.lock")).unwrap();
    lock.lines()
        .find_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("haproxy:3.4.7-alpine3.24")).then(|| w.next().unwrap().to_owned())
        })
        .expect("haproxy pinned in deploy/images.lock")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Haproxy(String);

impl Drop for Haproxy {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker with host networking (nightly edge job)"]
async fn real_haproxy_headers_name_the_client_and_its_health_checks_are_accepted() {
    let (front, back) = (free_port(), free_port());
    let cfg = format!(
        "global\n  log stdout format raw local0\ndefaults\n  mode tcp\n  timeout connect 3s\n  timeout client 10s\n  timeout server 10s\n\
         frontend f\n  bind 127.0.0.1:{front}\n  default_backend b\n\
         backend b\n  option tcp-check\n  server s 127.0.0.1:{back} check check-send-proxy inter 500ms send-proxy-v2\n"
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("haproxy.cfg"), cfg).unwrap();

    // The service side: the edge accepter trusting loopback (HAProxy connects from 127.0.0.1).
    let (tx, mut rx) = mpsc::unbounded_channel();
    let bound = BoundListener::bind(ListenerSpec { name: "compat", addr: SocketAddr::from(([127, 0, 0, 1], back)), max_connections: 16, max_per_ip: 8 }).await.unwrap();
    let stop = CancellationToken::new();
    let s = stop.clone();
    let accept = Arc::new(EdgeAccept::new(vec!["127.0.0.0/8".parse().unwrap()], false));
    tokio::spawn(async move {
        let _ = bound.serve(accept, Arc::new(Probe(tx)), Arc::new(TracingEvents), s, Duration::from_secs(2)).await;
    });

    let name = format!("rusty-haproxy-compat-{}", std::process::id());
    let st = Command::new("docker")
        .args(["run", "-d", "--rm", "--name", &name, "--network", "host", "-v"])
        .arg(format!("{}:/cfg:ro", dir.path().display()))
        .arg(image())
        .args(["haproxy", "-f", "/cfg/haproxy.cfg"])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success(), "docker run");
    let _guard = Haproxy(name);

    // A client from 127.0.0.2 connects through HAProxy (retry while it starts; its health checks meanwhile arrive as LOCAL headers).
    let mut conn = None;
    for _ in 0..40 {
        let sock = TcpSocket::new_v4().unwrap();
        sock.bind(SocketAddr::from(([127, 0, 0, 2], 0))).unwrap();
        if let Ok(c) = sock.connect(SocketAddr::from(([127, 0, 0, 1], front))).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let mut c = conn.expect("HAProxy did not come up");
    c.write_all(b"hello through haproxy\r\n").await.unwrap();
    let mut seen = None;
    while let Ok(Some((peer, line))) = tokio::time::timeout(Duration::from_secs(8), rx.recv()).await {
        // The health-check sessions (LOCAL) send nothing and keep HAProxy's own address: skip them.
        if line == "hello through haproxy" {
            seen = Some(peer);
            break;
        }
    }
    let peer = seen.expect("the client's session reached the service with its line");
    assert_eq!(peer.addr.ip().to_string(), "127.0.0.2", "the peer is the client behind HAProxy, not HAProxy");
    assert!(!peer.tls);
    stop.cancel();
}
