//! The all-in-one daemon as a library: start-up rules, the listeners it opens, graceful shutdown
//! with the store closed cleanly, and the composition-root wiring.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::*;
use rusty::daemon::{start, Running};
use rusty_core::config::{Listen, Profile, StartupConfig};
use rusty_core::Error;
use rusty_store::BLOB_ROOT_MARKER;
use rusty_svc::ListenerKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

fn addr_of(r: &Running, kind: ListenerKind) -> SocketAddr {
    r.addrs.iter().find(|(k, _)| *k == kind).map(|(_, a)| *a).expect("listener present")
}

async fn read_line(s: &mut (impl AsyncReadExt + Unpin)) -> String {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    let read = async {
        loop {
            if s.read(&mut b).await.unwrap_or(0) == 0 {
                break;
            }
            out.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), read).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn dev_start_opens_every_listener_creates_the_data_files_and_serves_banners() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let cfg = pg.config(&data);
    let token = CancellationToken::new();
    let run = start(cfg.clone(), Profile::Dev, token.clone()).await.unwrap();
    assert_eq!(run.addrs.len(), 5);

    // data dir 0700, blob root initialised with its marker and 0700
    assert_eq!(mode_of(&data), 0o700);
    assert_eq!(mode_of(&cfg.blob_dir), 0o700);
    assert!(cfg.blob_dir.join(BLOB_ROOT_MARKER).exists());

    // STARTTLS ports speak plaintext first; port 25 and 143 included
    let mut c = TcpStream::connect(addr_of(&run, ListenerKind::Smtp25)).await.unwrap();
    assert!(read_line(&mut c).await.starts_with("220 "), "smtp banner");
    let mut c = TcpStream::connect(addr_of(&run, ListenerKind::Imap143)).await.unwrap();
    assert!(read_line(&mut c).await.starts_with("* OK"), "imap banner");

    token.cancel();
    run.wait().await.unwrap();
}

#[tokio::test]
async fn every_service_the_daemon_runs_sends_a_heartbeat() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let token = CancellationToken::new();
    let run = start(pg.config(&data), Profile::Dev, token.clone()).await.unwrap();
    let store = pg.open_store().await;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..50 {
        seen = store.repos().heartbeat.list().await.unwrap().into_iter().map(|h| h.service).collect();
        if seen.len() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    seen.sort();
    assert_eq!(seen, ["gc", "imap", "outbound", "smtp-in", "submission"], "one heartbeat per service of the process");
    token.cancel();
    run.wait().await.unwrap();
}

#[tokio::test]
async fn production_start_without_hostname_or_tls_is_refused_and_binds_nothing() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");

    // no hostname
    let pg = pg_dev().await;
    let mut cfg = pg.config(&data);
    cfg.hostname = None;
    match start(cfg, Profile::Production, CancellationToken::new()).await {
        Err(Error::Invalid(m)) => assert!(m.contains("hostname"), "{m}"),
        other => panic!("{:?}", other.map(|_| ())),
    }

    // hostname but no certificate, dev cert not requested: authenticated listeners refuse to start
    let mut cfg = pg.config(&data);
    cfg.tls.dev_self_signed = false;
    cfg.hostname = Some("mail.example.test".into());
    match start(cfg, Profile::Production, CancellationToken::new()).await {
        Err(Error::Invalid(m)) => assert!(m.contains("TLS"), "{m}"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    // nothing was created: a refused start leaves the disk alone
    assert!(!data.exists(), "a refused start must not create the data dir");
}

#[tokio::test]
async fn port_25_alone_may_run_without_a_certificate() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let mut cfg = pg.config(&data);
    cfg.tls.dev_self_signed = false;
    cfg.hostname = Some("mail.example.test".into());
    cfg.smtp.listen_587 = Listen::off();
    cfg.smtp.listen_465 = Listen::off();
    cfg.imap.listen_143 = Listen::off();
    cfg.imap.listen_993 = Listen::off();
    // Production never creates directories: prepare them as an operator would.
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    rusty_store::FsBlobStore::init(&cfg.blob_dir).await.unwrap();
    let token = CancellationToken::new();
    let run = start(cfg, Profile::Production, token.clone()).await.unwrap();
    assert_eq!(run.addrs.len(), 1);
    assert_eq!(run.addrs[0].0, ListenerKind::Smtp25);
    token.cancel();
    run.wait().await.unwrap();
}

#[tokio::test]
async fn dev_profile_refuses_an_existing_data_dir_that_others_can_enter() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    let pg = pg_dev().await;
    match start(pg.config(&data), Profile::Dev, CancellationToken::new()).await {
        Err(Error::Invalid(m)) => assert!(m.contains("0700"), "{m}"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    // a symlinked data dir is refused too
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(start(pg.config(&link), Profile::Dev, CancellationToken::new()).await.is_err());
}

#[tokio::test]
async fn a_port_clash_fails_the_start_and_leaves_the_first_instance_running() {
    let tmp = private_tmp();
    let pg = pg_dev().await;
    let a = start(pg.config(&tmp.path().join("a")), Profile::Dev, CancellationToken::new()).await.unwrap();
    let taken = addr_of(&a, ListenerKind::Smtp25);
    let mut cfg = pg.config(&tmp.path().join("b"));
    cfg.smtp.listen_25 = Listen::on(taken);
    assert!(start(cfg, Profile::Dev, CancellationToken::new()).await.is_err());
    // the first one still answers
    let mut c = TcpStream::connect(taken).await.unwrap();
    assert!(read_line(&mut c).await.starts_with("220 "));
    a.shutdown();
    a.wait().await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_idle_sessions_and_closes_the_listeners() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let cfg = pg.config(&data);
    let token = CancellationToken::new();
    let run = start(cfg.clone(), Profile::Dev, token.clone()).await.unwrap();
    let smtp = addr_of(&run, ListenerKind::Smtp25);
    let imap = addr_of(&run, ListenerKind::Imap143);

    // two idle sessions that have read their banner
    let mut s1 = TcpStream::connect(smtp).await.unwrap();
    let mut s2 = TcpStream::connect(imap).await.unwrap();
    read_line(&mut s1).await;
    read_line(&mut s2).await;

    let t0 = Instant::now();
    token.cancel();
    // both are told goodbye (drained, not cut)
    assert!(read_line(&mut s1).await.starts_with("421 "), "smtp goodbye");
    assert!(read_line(&mut s2).await.starts_with("* BYE"), "imap goodbye");
    run.wait().await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(5), "shutdown took {:?}", t0.elapsed());

    // listeners are closed
    assert!(TcpStream::connect(smtp).await.is_err());
    assert!(TcpStream::connect(imap).await.is_err());

    // the pool was closed and the database is still reachable by a fresh open
    pg.open_store().await.close().await.unwrap();
}

#[tokio::test]
async fn the_imap_listener_serves_real_imap() {
    let tmp = private_tmp();
    let token = CancellationToken::new();
    let pg = pg_dev().await;
    let run = start(pg.config(&tmp.path().join("d")), Profile::Dev, token.clone()).await.unwrap();
    let mut c = TcpStream::connect(addr_of(&run, ListenerKind::Imap143)).await.unwrap();
    read_line(&mut c).await;
    c.write_all(b"x1 NOOP\r\n").await.unwrap();
    let l = read_line(&mut c).await;
    assert!(l.starts_with("x1 OK"), "{l}");
    c.write_all(b"x2 LOGIN a b\r\n").await.unwrap();
    let l = read_line(&mut c).await;
    assert!(l.contains("PRIVACYREQUIRED"), "plaintext login is refused: {l}");
    token.cancel();
    run.wait().await.unwrap();
}

#[test]
fn the_listener_plan_follows_the_configuration() {
    let mut c = StartupConfig::dev(std::path::Path::new("/tmp/x"));
    let plan = rusty_svc::listener_plan(&c);
    let kinds: Vec<_> = plan.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ListenerKind::Smtp25,
            ListenerKind::Submission587,
            ListenerKind::Submissions465,
            ListenerKind::Imap143,
            ListenerKind::Imaps993
        ]
    );
    assert!(plan.iter().filter(|p| p.kind.implicit_tls()).count() == 2);
    assert!(!ListenerKind::Smtp25.accepts_auth() && ListenerKind::Imap143.accepts_auth());
    c.smtp.listen_587 = Listen::off();
    c.imap.listen_143 = Listen::off();
    assert_eq!(rusty_svc::listener_plan(&c).len(), 3);
}

#[test]
fn port_offset_and_bind_rewrite_every_dev_listener() {
    let c = StartupConfig::dev(std::path::Path::new("/tmp/x"));
    let moved = rusty::daemon::with_port_offset(c.clone(), 100).unwrap();
    assert_eq!(moved.smtp.listen_25.addr(), Some("127.0.0.1:2625".parse().unwrap()));
    assert_eq!(moved.imap.listen_993.addr(), Some("127.0.0.1:10093".parse().unwrap()));
    assert!(rusty::daemon::with_port_offset(c.clone(), 60_000).is_err(), "9993 + 60000 does not fit a port");
    let all = rusty::daemon::with_bind(c, "0.0.0.0".parse().unwrap());
    assert_eq!(all.smtp.listen_587.addr(), Some("0.0.0.0:5587".parse().unwrap()));
    assert_eq!(all.imap.listen_143.addr(), Some("0.0.0.0:1143".parse().unwrap()));
}

/// One authenticated submission over implicit TLS (465) up to the answer to `RCPT TO:<rcpt>`.
fn submit_rcpt(addr: SocketAddr, rcpt: &str) -> String {
    use base64::Engine;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Arc;
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(&self, _: &rustls::pki_types::CertificateDer<'_>, _: &[rustls::pki_types::CertificateDer<'_>], _: &rustls::pki_types::ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
        }
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let conn = rustls::ClientConnection::new(Arc::new(cfg), "localhost".try_into().unwrap()).unwrap();
    let sock = std::net::TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut io = BufReader::new(rustls::StreamOwned::new(conn, sock));
    let reply = |io: &mut BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>| {
        let mut last;
        loop {
            let mut l = String::new();
            io.read_line(&mut l).unwrap();
            let done = l.as_bytes().get(3) != Some(&b'-');
            last = l;
            if done {
                return last;
            }
        }
    };
    let cmd = |io: &mut BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>, line: &str| {
        io.get_mut().write_all(format!("{line}\r\n").as_bytes()).unwrap();
        io.get_mut().flush().unwrap();
        reply(io)
    };
    assert!(reply_first(&mut io).starts_with("220"));
    assert!(cmd(&mut io, "EHLO t.example").starts_with("250"));
    let blob = base64::engine::general_purpose::STANDARD.encode("\0alice@example.net\0pw-alice-1234");
    assert!(cmd(&mut io, &format!("AUTH PLAIN {blob}")).starts_with("235"));
    assert!(cmd(&mut io, "MAIL FROM:<alice@example.net>").starts_with("250"));
    cmd(&mut io, &format!("RCPT TO:<{rcpt}>"))
}

fn reply_first(io: &mut std::io::BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>) -> String {
    use std::io::BufRead;
    let mut l = String::new();
    io.read_line(&mut l).unwrap();
    l
}

async fn seed_alice(pg: &PgDev) {
    let store = pg.open_store().await;
    let dir = rusty_directory::Directory::with_repo(store.repos().directory.clone());
    let dom = dir.create_domain("example.net").await.unwrap();
    dir.create_account("alice", dom, Some("pw-alice-1234"), None).await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_recipients_are_accepted_only_while_the_outbound_worker_is_enabled() {
    for enabled in [true, false] {
        let tmp = private_tmp();
        let data = tmp.path().join("data");
        let pg = pg_dev().await;
        let mut cfg = pg.config(&data);
        cfg.outbound.enabled = enabled;
        let token = CancellationToken::new();
        let run = start(cfg.clone(), Profile::Dev, token.clone()).await.unwrap();
        seed_alice(&pg).await;
        let addr = addr_of(&run, ListenerKind::Submissions465);
        let reply = tokio::task::spawn_blocking(move || submit_rcpt(addr, "someone@remote.test")).await.unwrap();
        if enabled {
            assert!(reply.starts_with("250"), "worker enabled: queued ({reply})");
        } else {
            assert!(reply.starts_with("451 4.3.2"), "worker disabled: never accepted into a queue nobody drains ({reply})");
        }
        token.cancel();
        run.wait().await.unwrap();
    }
}

#[tokio::test]
async fn the_dev_certificate_is_kept_in_the_data_dir_across_restarts() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let cfg = pg.config(&data);
    let mut leaves = Vec::new();
    for _ in 0..2 {
        let token = CancellationToken::new();
        let run = start(cfg.clone(), Profile::Dev, token.clone()).await.unwrap();
        leaves.push(run.tls().leaf_der().unwrap());
        token.cancel();
        run.wait().await.unwrap();
    }
    assert_eq!(leaves[0], leaves[1], "the same certificate after a restart");
    assert_eq!(mode_of(&data.join("dev-tls-key.pem")), 0o600);
}

#[tokio::test]
async fn a_service_started_for_its_role_binds_only_its_own_ports_and_the_others_bind_none() {
    use rusty_core::config::Role;
    use rusty_svc::ListenerKind::*;
    let pg = pg_dev().await;
    let cases: [(Role, Vec<rusty_svc::ListenerKind>); 5] = [
        (Role::SmtpIn, vec![Smtp25]),
        (Role::Submission, vec![Submission587, Submissions465]),
        (Role::Imap, vec![Imap143, Imaps993]),
        (Role::Outbound, vec![]),
        (Role::Gc, vec![]),
    ];
    for (role, want) in cases {
        let tmp = private_tmp();
        let cfg = pg.config(&tmp.path().join("data"));
        let token = CancellationToken::new();
        let run = rusty::daemon::start_role(cfg, Profile::Dev, Some(role), token.clone()).await.unwrap();
        let mut got: Vec<_> = run.addrs.iter().map(|(k, _)| k.name()).collect();
        let mut want: Vec<_> = want.iter().map(|k| k.name()).collect();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "{}", role.name());
        token.cancel();
        run.wait().await.unwrap();
    }
}

#[test]
fn only_the_listeners_of_a_role_need_tls_and_count_as_services() {
    use rusty_core::config::Role;
    let mut cfg = StartupConfig::dev(std::path::Path::new("/tmp/rusty-roles"));
    cfg.tls.dev_self_signed = false;
    assert!(cfg.check_startup_for(Profile::Dev, Some(Role::SmtpIn)).is_ok(), "port 25 offers STARTTLS only when it can; it needs no certificate");
    assert!(cfg.check_startup_for(Profile::Dev, Some(Role::Gc)).is_ok(), "a service with no port needs no certificate");
    assert!(cfg.check_startup_for(Profile::Dev, Some(Role::Outbound)).is_ok());
    assert!(cfg.check_startup_for(Profile::Dev, Some(Role::Imap)).is_err(), "imap has authenticated listeners and no certificate");
    assert!(cfg.check_startup_for(Profile::Dev, Some(Role::Submission)).is_err());
    assert!(cfg.check_startup_for(Profile::Dev, None).is_err());
    assert_eq!(cfg.tls_required_listeners_for(Some(Role::Imap)), 2);
    assert_eq!(cfg.tls_required_listeners_for(Some(Role::SmtpIn)), 0);
    assert_eq!(cfg.service_count_for(Some(Role::Gc)), 2);
    assert_eq!(cfg.service_count_for(Some(Role::Imap)), 4);
    assert_eq!(cfg.service_count_for(None), 7);
}

#[tokio::test]
async fn the_metrics_listener_counts_what_the_service_listeners_report_and_answers_the_allowed_scraper_only() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let mut cfg = pg.config(&data);
    // A free port for the metrics listener (the service logs it; the test needs it before the start).
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    cfg.metrics = Some(rusty_core::config::MetricsConfig { listen: format!("127.0.0.1:{port}").parse().unwrap(), allowed_peers: vec!["127.0.0.1/32".parse().unwrap()] });
    let token = CancellationToken::new();
    let run = start(cfg, Profile::Dev, token.clone()).await.unwrap();
    // One connection to the SMTP port, so there is something to count.
    let mut c = TcpStream::connect(addr_of(&run, ListenerKind::Smtp25)).await.unwrap();
    assert!(read_line(&mut c).await.starts_with("220 "));
    drop(c);
    let mut text = String::new();
    for _ in 0..30 {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await;
        text = String::from_utf8_lossy(&out).into_owned();
        if text.contains("rusty_connections_closed_total") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("rusty_build_info{service=\"rusty\","), "{text}");
    assert!(text.contains("rusty_connections_accepted_total{listener=\"smtp-25\"} 1"), "{text}");
    assert!(text.contains("rusty_connections_closed_total{listener=\"smtp-25\"} 1"), "{text}");
    token.cancel();
    run.wait().await.unwrap();
}
