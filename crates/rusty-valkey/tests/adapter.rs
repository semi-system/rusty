//! What only the Valkey implementation can get wrong: the login, a server that is gone, key namespaces, and
//! that nothing secret or server-identifying reaches an error, a log line or a `Debug`.

mod common;

use std::time::{Duration, Instant};

use common::{options, parts, url};
use rusty_core::Error;
use rusty_valkey::{Kv, TlsOptions, ValkeyKv, ValkeyOptions};

fn must_be_unavailable(e: &Error, secrets: &[&str]) {
    let Error::Busy(msg) = e else { panic!("expected Busy, got {e:?}") };
    assert_eq!(msg, "key-value store unavailable");
    let all = format!("{e} {e:?}");
    for s in secrets {
        assert!(!all.contains(s), "the error leaks {s:?}: {all}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_is_refused_without_echoing_anything() {
    let Some(good) = options("auth") else { return };
    let (host, port, user, _) = parts(&url().unwrap());
    let bad = ValkeyOptions { password: Some("not-the-password-42".into()), ..good.clone() };
    let e = match ValkeyKv::connect(bad).await {
        Ok(_) => panic!("the server accepted a wrong password"),
        Err(e) => e,
    };
    must_be_unavailable(&e, &["not-the-password-42", &host, &user, &port.to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_user_is_refused() {
    let Some(good) = options("auth") else { return };
    let bad = ValkeyOptions { username: Some("nobody".into()), ..good };
    assert!(ValkeyKv::connect(bad).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_is_gone_fails_fast_and_never_hangs() {
    // A port nothing listens on: bind, note the port, close.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let o = ValkeyOptions {
        host: "127.0.0.1".into(),
        port,
        tls: None,
        username: None,
        password: Some("pw-that-must-not-leak".into()),
        connect_timeout: Duration::from_millis(500),
        command_timeout: Duration::from_millis(200),
        key_prefix: "x:".into(),
    };
    let t0 = Instant::now();
    let e = match ValkeyKv::connect(o).await {
        Ok(_) => panic!("connected to a port nobody listens on"),
        Err(e) => e,
    };
    assert!(t0.elapsed() < Duration::from_secs(5), "connect gave up only after {:?}", t0.elapsed());
    must_be_unavailable(&e, &["pw-that-must-not-leak", &port.to_string()]);
}

/// A running connection loses its server: every call then fails within its bound and says "unavailable".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_fail_within_their_bound_while_the_server_is_unreachable_and_recover_afterwards() {
    let Some(base) = options("outage") else { return };
    let (host, real_port, ..) = parts(&url().unwrap());
    // A tiny forwarder in front of the real server that we can stop and start again.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    drop(listener);
    let proxy = Proxy::start(proxy_port, format!("{host}:{real_port}")).await;
    let o = ValkeyOptions { host: "127.0.0.1".into(), port: proxy_port, command_timeout: Duration::from_millis(300), ..base };
    let kv = ValkeyKv::connect(o).await.unwrap();
    kv.set("k", b"v", Duration::from_secs(30)).await.unwrap();

    proxy.stop().await; // the link drops, the port stops answering
    let t0 = Instant::now();
    let e = kv.get("k").await.unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(3), "a call took {:?} against a dead server", t0.elapsed());
    must_be_unavailable(&e, &[&real_port.to_string()]);

    let proxy = Proxy::start(proxy_port, format!("{host}:{real_port}")).await;
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if kv.get("k").await.map(|v| v.as_deref() == Some(&b"v"[..])).unwrap_or(false) {
            break;
        }
        assert!(Instant::now() < end, "the client never found the server again");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_prefixes_keep_installations_apart() {
    let (Some(a), Some(b)) = (options("tenant-a"), options("tenant-b")) else { return };
    let (a, b) = (ValkeyKv::connect(a).await.unwrap(), ValkeyKv::connect(b).await.unwrap());
    a.set("shared-name", b"from a", Duration::from_secs(30)).await.unwrap();
    assert_eq!(b.get("shared-name").await.unwrap(), None);
    assert_eq!(a.get("shared-name").await.unwrap().as_deref(), Some(&b"from a"[..]));
    let w = Duration::from_secs(30);
    assert!(a.window_try_record("w", w, 1).await.unwrap().allowed);
    assert!(b.window_try_record("w", w, 1).await.unwrap().allowed, "b has a window of its own");
}

#[test]
fn debug_output_never_shows_the_password() {
    let o = ValkeyOptions {
        host: "h".into(),
        port: 6379,
        tls: None,
        username: Some("u".into()),
        password: Some("super-secret-pw".into()),
        connect_timeout: Duration::from_secs(1),
        command_timeout: Duration::from_millis(200),
        key_prefix: "p:".into(),
    };
    let shown = format!("{o:?}");
    assert!(!shown.contains("super-secret-pw"), "{shown}");
    assert!(shown.contains("redacted"));
}

#[tokio::test]
async fn a_bad_ca_bundle_is_refused_before_any_connection_is_tried() {
    for pem in [&b""[..], b"not a pem", b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"] {
        let o = ValkeyOptions {
            host: "127.0.0.1".into(),
            port: 1,
            tls: Some(TlsOptions { ca_bundle_pem: Some(pem.to_vec()) }),
            username: None,
            password: None,
            connect_timeout: Duration::from_millis(200),
            command_timeout: Duration::from_millis(200),
            key_prefix: "x:".into(),
        };
        let started = Instant::now();
        let e = match ValkeyKv::connect(o).await {
            Ok(_) => panic!("accepted an unusable CA bundle"),
            Err(e) => e,
        };
        assert!(matches!(e, Error::Invalid(_)), "{e:?} for {pem:?}");
        assert!(started.elapsed() < Duration::from_millis(150), "the bundle was checked only after trying to connect");
    }
}

/// A TCP forwarder that can be stopped (drops every connection and frees the port) and started again.
struct Proxy {
    stop: tokio::sync::oneshot::Sender<()>,
    done: tokio::task::JoinHandle<()>,
}

impl Proxy {
    async fn start(port: u16, target: String) -> Proxy {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let done = tokio::spawn(async move {
            let mut conns = Vec::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let Ok((mut inbound, _)) = accepted else { continue };
                        let target = target.clone();
                        conns.push(tokio::spawn(async move {
                            if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                            }
                        }));
                    }
                }
            }
            for c in conns {
                c.abort();
            }
        });
        Proxy { stop, done }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.done.await;
    }
}
