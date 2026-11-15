//! The daemon behind an edge proxy (PROXY protocol v2): the client address from the header is what the authentication throttle sees, a connection that is
//! not from a trusted proxy is dropped, and the implicit-TLS ports start without a certificate of their own (the edge holds that key) while the STARTTLS
//! ports still need one.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::*;
use rusty::daemon::start;
use rusty_core::config::Profile;
use rusty_svc::proxy::encode;
use rusty_svc::ListenerKind;
use tokio_util::sync::CancellationToken;

fn imap_via_proxy(addr: SocketAddr, client: &str, user: &str, pw: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&encode(client.parse().unwrap(), "192.0.2.1:143".parse().unwrap(), false)).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut greeting = String::new();
    r.read_line(&mut greeting).unwrap();
    // Plain-text LOGIN is refused before TLS on this port: the answer still tells whether the attempt was throttled or checked.
    s.write_all(format!("a LOGIN \"{user}\" \"{pw}\"\r\n").as_bytes()).unwrap();
    let mut answer = String::new();
    let _ = r.read_line(&mut answer);
    format!("{greeting}{answer}")
}

fn addr_of(r: &rusty::daemon::Running, kind: ListenerKind) -> SocketAddr {
    r.addrs.iter().find(|(k, _)| *k == kind).map(|(_, a)| *a).expect("listener present")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn behind_the_edge_the_header_names_the_client_and_strangers_are_dropped() {
    let tmp = private_tmp();
    let pg = pg_dev().await;
    let mut cfg = pg.config(&tmp.path().join("data"));
    cfg.imap.proxy_protocol = true;
    cfg.imap.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()];
    let token = CancellationToken::new();
    let run = start(cfg, Profile::Dev, token.clone()).await.unwrap();
    let imap = addr_of(&run, ListenerKind::Imap143);
    tokio::task::spawn_blocking(move || {
        // A header from the trusted proxy: the session starts and speaks IMAP.
        let a = imap_via_proxy(imap, "203.0.113.9:4000", "x@y.test", "pw");
        assert!(a.starts_with("* OK"), "{a}");
        // A client that speaks IMAP without the header is not a proxy: the greeting is never sent (the header is read first and refused).
        let mut s = TcpStream::connect(imap).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let _ = s.write_all(b"a NOOP\r\n");
        let mut line = String::new();
        let n = BufReader::new(s).read_line(&mut line).unwrap_or(0);
        assert!(n == 0 || !line.starts_with("* OK"), "a connection without a PROXY header must not get a session: {line:?}");
    })
    .await
    .unwrap();
    token.cancel();
    run.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn implicit_tls_ports_behind_the_edge_start_without_a_certificate_but_starttls_ports_do_not() {
    let tmp = private_tmp();
    let pg = pg_dev().await;
    // Only the implicit-TLS ports (465, 993) of services behind the edge, and no certificate at all: allowed.
    let mut cfg = pg.config(&tmp.path().join("data"));
    cfg.tls.dev_self_signed = false;
    cfg.smtp.listen_587 = rusty_core::config::Listen::off();
    cfg.imap.listen_143 = rusty_core::config::Listen::off();
    cfg.smtp.proxy_protocol = true;
    cfg.smtp.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()];
    cfg.imap.proxy_protocol = true;
    cfg.imap.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()];
    assert!(cfg.check_startup(Profile::Dev).is_ok());
    // The STARTTLS ports are upgraded by the service itself: with one of them enabled and no certificate the start is refused.
    cfg.imap.listen_143 = rusty_core::config::Listen::on("127.0.0.1:0".parse().unwrap());
    assert!(cfg.check_startup(Profile::Dev).is_err());
    // And without the proxy protocol an implicit-TLS port needs the service's own certificate, as before.
    cfg.imap.listen_143 = rusty_core::config::Listen::off();
    cfg.imap.proxy_protocol = false;
    cfg.imap.trusted_proxies.clear();
    assert!(cfg.check_startup(Profile::Dev).is_err());
}
