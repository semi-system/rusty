//! Greeting, capabilities, LOGIN / AUTHENTICATE, STARTTLS, throttling and framing limits, as
//! golden transcripts against a real `ImapHandler`.

mod common;

use std::sync::Arc;

use common::*;
use rusty_imap::Limits;
use tokio::io::AsyncWriteExt;

fn caps(line: &str) -> Vec<&str> {
    let inner = line.split("[CAPABILITY ").nth(1).unwrap().split(']').next().unwrap();
    inner.split(' ').collect()
}

#[tokio::test]
async fn greeting_advertises_starttls_and_disables_login_before_tls() {
    for_each_backend(|b| async move {
        let mut c = Conn::open(handler(b.ctx().await), false);
        let g = c.line().await;
        assert!(g.starts_with("* OK [CAPABILITY IMAP4rev1 "), "{g}");
        let c1 = caps(&g);
        assert!(c1.contains(&"STARTTLS") && c1.contains(&"LOGINDISABLED"), "{g}");
        assert!(!c1.iter().any(|x| x.starts_with("AUTH=")), "no AUTH before TLS: {g}");
        let lines = c.cmd("CAPABILITY").await;
        assert!(lines[0].starts_with("* CAPABILITY IMAP4rev1 ") && lines[1] == "t1 OK CAPABILITY completed", "{lines:?}");
        // LOGIN is refused before TLS, with the password never checked.
        b.account("alice@a.test", "pw-alice").await;
        let st = c.status("LOGIN \"alice@a.test\" \"pw-alice\"").await;
        assert!(st.starts_with("t2 NO [PRIVACYREQUIRED]"), "{st}");
        let st = c.status("AUTHENTICATE PLAIN AGFsaWNlQGEudGVzdABwdy1hbGljZQ==").await;
        assert!(st.starts_with("t3 NO [PRIVACYREQUIRED]"), "{st}");
    })
    .await;
}

#[tokio::test]
async fn without_a_certificate_there_is_no_starttls_and_no_login() {
    for_each_backend(|b| async move {
        let mut c = Conn::open(handler(b.ctx_without_tls().await), false);
        let g = c.line().await;
        let c1 = caps(&g);
        assert!(!c1.contains(&"STARTTLS") && c1.contains(&"LOGINDISABLED"), "{g}");
        assert!(c.status("STARTTLS").await.starts_with("t1 NO [CANNOT]"));
    })
    .await;
}

#[tokio::test]
async fn implicit_tls_offers_auth_plain_and_logs_in() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let mut c = Conn::open(handler(b.ctx().await), true);
        let g = c.line().await;
        let c1 = caps(&g);
        assert!(c1.contains(&"AUTH=PLAIN") && !c1.contains(&"LOGINDISABLED") && !c1.contains(&"STARTTLS"), "{g}");
        let lines = c.cmd("LOGIN \"alice@a.test\" \"pw-alice\"").await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("t1 OK [CAPABILITY IMAP4rev1 "), "{lines:?}");
        assert!(!caps(&lines[0].replace("t1 OK ", "* OK ")).contains(&"AUTH=PLAIN"), "no AUTH once logged in");
        // Authenticated: SELECT is possible, a second LOGIN is not.
        assert!(c.status("LOGIN \"alice@a.test\" \"pw-alice\"").await.starts_with("t2 BAD"));
        assert!(c.status("SELECT INBOX").await.contains("OK [READ-WRITE]"));
    })
    .await;
}

#[tokio::test]
async fn wrong_credentials_are_refused_without_saying_which_part_was_wrong() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let mut c = Conn::open(handler(b.ctx().await), true);
        c.line().await;
        let a = c.status("LOGIN \"alice@a.test\" \"wrong\"").await;
        let u = c.status("LOGIN \"nobody@a.test\" \"wrong\"").await;
        assert_eq!(a.split_once(' ').unwrap().1, u.split_once(' ').unwrap().1, "same answer for a wrong password and an unknown user");
        assert!(a.contains("NO [AUTHENTICATIONFAILED]"), "{a}");
        // Not logged in: mailbox commands are refused.
        assert!(c.status("SELECT INBOX").await.contains("NO [AUTHENTICATIONREQUIRED]"));
        assert!(c.status("FETCH 1 FLAGS").await.contains("NO [AUTHENTICATIONREQUIRED]"));
    })
    .await;
}

#[tokio::test]
async fn authenticate_plain_with_and_without_an_initial_response() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let ctx = b.ctx().await;
        // "\0alice@a.test\0pw-alice"
        let blob = "AGFsaWNlQGEudGVzdABwdy1hbGljZQ==";
        let mut c = Conn::open(handler(ctx.clone()), true);
        c.line().await;
        let st = c.status(&format!("AUTHENTICATE PLAIN {blob}")).await;
        assert!(st.starts_with("t1 OK [CAPABILITY"), "{st}");

        // Without SASL-IR: continuation, then the payload.
        let mut c = Conn::open(handler(ctx.clone()), true);
        c.line().await;
        c.send("a1 AUTHENTICATE PLAIN\r\n").await;
        assert_eq!(c.line().await, "+ ");
        c.send(&format!("{blob}\r\n")).await;
        assert!(c.line().await.starts_with("a1 OK [CAPABILITY"));

        // Cancelled with `*`.
        let mut c = Conn::open(handler(ctx.clone()), true);
        c.line().await;
        c.send("a1 AUTHENTICATE PLAIN\r\n").await;
        c.line().await;
        c.send("*\r\n").await;
        assert!(c.line().await.starts_with("a1 BAD"));
        assert!(c.status("NOOP").await.starts_with("t1 OK"), "the session goes on after a cancelled attempt");

        // Invalid base64, wrong password, wrong mechanism.
        let mut c = Conn::open(handler(ctx.clone()), true);
        c.line().await;
        c.send("a1 AUTHENTICATE PLAIN\r\n").await;
        c.line().await;
        c.send("!!not-base64!!\r\n").await;
        assert!(c.line().await.starts_with("a1 BAD"));
        assert!(c.status("AUTHENTICATE PLAIN AGFsaWNlQGEudGVzdABub3B3").await.contains("NO [AUTHENTICATIONFAILED]"));
        assert!(c.status("AUTHENTICATE CRAM-MD5").await.contains("NO [CANNOT]"));
        // authzid different from authcid is refused ("bob\0alice@a.test\0pw-alice").
        assert!(c.status("AUTHENTICATE PLAIN Ym9iAGFsaWNlQGEudGVzdABwdy1hbGljZQ==").await.contains("NO [AUTHENTICATIONFAILED]"));
    })
    .await;
}

#[tokio::test]
async fn too_many_failures_block_even_the_right_password_and_close_the_connection() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let ctx = b.ctx_with(|c| c.limits.auth_failures_max = 3).await;
        let h = handler(ctx);
        let mut c = Conn::open(h.clone(), true);
        c.line().await;
        for _ in 0..3 {
            assert!(c.status("LOGIN \"alice@a.test\" \"wrong\"").await.contains("NO [AUTHENTICATIONFAILED]"));
        }
        // The throttle (peer, identity) now blocks the correct password too.
        let st = c.status("LOGIN \"alice@a.test\" \"pw-alice\"").await;
        assert!(st.contains("NO [AUTHENTICATIONFAILED]") && st.contains("Too many"), "{st}");
        // Another connection of the same peer is blocked as well.
        let mut d = Conn::open(h, true);
        d.line().await;
        assert!(d.status("LOGIN \"alice@a.test\" \"pw-alice\"").await.contains("Too many"));
    })
    .await;
}

#[tokio::test]
async fn a_connection_that_keeps_failing_is_closed() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let ctx = b.ctx_with(|c| c.limits.auth_failures_max = 1000).await;
        let limits = Limits { auth_failures: 3, ..Limits::default() };
        let mut c = Conn::open(handler_with(ctx, limits), true);
        c.line().await;
        for _ in 0..2 {
            c.status("LOGIN \"alice@a.test\" \"wrong\"").await;
        }
        let lines = c.cmd("LOGIN \"alice@a.test\" \"wrong\"").await;
        assert!(lines.iter().any(|l| l.starts_with("* BYE")), "{lines:?}");
        assert!(c.closed().await);
    })
    .await;
}

#[tokio::test]
async fn starttls_upgrades_and_then_authentication_works() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let mut c = Conn::open(handler(b.ctx().await), false);
        c.line().await;
        let st = c.status("STARTTLS").await;
        assert!(st.starts_with("t1 OK Begin TLS negotiation now"), "{st}");
        tls_upgrade(&mut c).await;
        // Capabilities are asked again after the upgrade (RFC 3501 6.2.1).
        let lines = c.cmd("CAPABILITY").await;
        assert!(lines[0].contains("AUTH=PLAIN") && !lines[0].contains("STARTTLS") && !lines[0].contains("LOGINDISABLED"), "{lines:?}");
        c.login("alice@a.test", "pw-alice").await;
        assert!(c.status("NOOP").await.contains("OK"));
    })
    .await;
}

#[tokio::test]
async fn data_pipelined_behind_starttls_is_not_executed() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let mut c = Conn::open(handler(b.ctx().await), false);
        c.line().await;
        // A man in the middle appends a plaintext command to the STARTTLS line.
        c.send("a1 STARTTLS\r\na2 LOGIN \"alice@a.test\" \"pw-alice\"\r\n").await;
        let mut seen = vec![];
        while let Some(l) = c.try_line().await {
            seen.push(l);
        }
        assert!(seen.iter().any(|l| l.starts_with("* BYE")), "{seen:?}");
        assert!(!seen.iter().any(|l| l.starts_with("a2 OK")), "the smuggled LOGIN ran: {seen:?}");
    })
    .await;
}

#[tokio::test]
async fn a_two_gigabyte_literal_is_refused_before_it_is_read() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw-alice").await;
        let mut c = Conn::open(handler(b.ctx().await), true);
        c.line().await;
        c.login("alice@a.test", "pw-alice").await;
        c.send("x1 APPEND INBOX {2147483648}\r\n").await;
        let l = c.line().await;
        assert!(l.starts_with("x1 NO [LIMIT]"), "{l}");
        // The connection is still usable: nothing was waiting for 2 GB of data.
        assert!(c.status("NOOP").await.contains("OK"));
    })
    .await;
}

#[tokio::test]
async fn an_oversized_command_line_closes_the_session() {
    for_each_backend(|b| async move {
        let mut c = Conn::open(handler(b.ctx().await), true);
        c.line().await;
        let mut big = b"a1 NOOP ".to_vec();
        big.extend(std::iter::repeat_n(b'x', 200_000));
        c.send_bytes(&big).await;
        let l = c.line().await;
        assert!(l.starts_with("* BYE"), "{l}");
        assert!(c.closed().await);
    })
    .await;
}

#[tokio::test]
async fn garbage_gets_bad_with_the_tag_and_the_session_survives() {
    for_each_backend(|b| async move {
        let mut c = Conn::open(handler(b.ctx().await), true);
        c.line().await;
        c.send("a1 FROBNICATE\r\n").await;
        assert!(c.line().await.starts_with("a1 BAD"));
        c.send("\r\n").await;
        assert!(c.line().await.starts_with("* BAD"));
        // Pipelined commands all get answers, in order.
        c.send("p1 NOOP\r\np2 CAPABILITY\r\np3 NOOP\r\n").await;
        assert!(c.line().await.starts_with("p1 OK"));
        assert!(c.line().await.starts_with("* CAPABILITY"));
        assert!(c.line().await.starts_with("p2 OK"));
        assert!(c.line().await.starts_with("p3 OK"));
        let lines = c.cmd("LOGOUT").await;
        assert_eq!(lines[0], "* BYE Logging out");
        assert!(lines[1].starts_with("t1 OK"));
        assert!(c.closed().await);
    })
    .await;
}

#[tokio::test]
async fn an_idle_unauthenticated_connection_is_closed() {
    for_each_backend(|b| async move {
        let limits = Limits { idle_unauthenticated: std::time::Duration::from_millis(200), ..Limits::default() };
        let mut c = Conn::open(handler_with(b.ctx().await, limits), true);
        c.line().await;
        let l = c.line().await;
        assert!(l.starts_with("* BYE") && l.contains("idle"), "{l}");
        assert!(c.closed().await);
    })
    .await;
}

#[tokio::test]
async fn shutdown_says_bye_and_ends_the_session() {
    for_each_backend(|b| async move {
        let mut c = Conn::open(handler(b.ctx().await), true);
        c.line().await;
        c.shutdown.cancel();
        assert_eq!(c.line().await, "* BYE Server shutting down");
        assert!(c.closed().await);
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut c.task).await.unwrap().unwrap();
    })
    .await;
}

/// Run the TLS client handshake over the connection's pipe (the server side upgrades itself).
async fn tls_upgrade(c: &mut Conn) {
    use rustls::pki_types::{CertificateDer, ServerName};
    use std::sync::Arc;
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let placeholder: Box<dyn Rw> = Box::new(tokio::io::duplex(1).0);
    let plain = std::mem::replace(&mut c.io, placeholder);
    let tls = connector.connect(ServerName::try_from("mail.test".to_owned()).unwrap(), plain).await.expect("tls handshake");
    c.io = Box::new(tls);
}

#[allow(unused_imports)]
use Arc as _;
#[allow(unused_imports)]
use AsyncWriteExt as _;
