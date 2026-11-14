#![allow(clippy::field_reassign_with_default)]
//! `check_relay`: the operator's "can I send through this relay?" check, against loopback fake relays.
//! It must do exactly what the worker does before it sends (resolve, connect, TLS, AUTH) and stop
//! there: no MAIL FROM, no message. Every failure is one fixed category with a fixed text, and no
//! text ever carries the password, the user name, the host or what the remote server said.
mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use common::fake_mta::*;
use rusty_core::config::{OutboundConfig, RelayConfig, RelayTls};
use rusty_smtp::outbound::dial::Dialer;
use rusty_smtp::outbound::relay_check::{check_relay, RelayCheckDeps, RelayFailure};
use rusty_smtp::outbound::tls::RustlsUpgrader;
use rusty_smtp::resolve::StaticDns;
use rusty_svc::FileSecretSource;

const HOST: &str = "relay.fake.test";

struct Rig {
    dir: tempfile::TempDir,
    dns: StaticDns,
    dialer: Arc<MapDialer>,
    cfg: OutboundConfig,
}

fn rig(tls: RelayTls, relay: &FakeMta, pki: TestPki) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let pw = dir.path().join("relay.pw");
    std::fs::write(&pw, "s3cret\n").unwrap();
    std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bundle = dir.path().join("ca.pem");
    std::fs::write(&bundle, &pki.ca_pem).unwrap();
    let mut cfg = OutboundConfig::default();
    cfg.ca_bundle = Some(bundle);
    cfg.connect_timeout_secs = 2;
    cfg.command_timeout_secs = 2;
    cfg.relay = Some(RelayConfig { host: HOST.into(), port: 587, tls, username: Some("mailer".into()), password_file: Some(pw) });
    let mut dns = StaticDns::default();
    dns.addrs.insert(HOST.into(), vec!["192.0.2.50".parse().unwrap()]);
    let dialer = MapDialer::new(&[("192.0.2.50", relay.addr)]);
    Rig { dir, dns, dialer, cfg }
}

async fn run(r: &Rig) -> Result<rusty_smtp::outbound::relay_check::RelayReport, RelayFailure> {
    let tls = RustlsUpgrader::new(r.cfg.ca_bundle.as_deref()).unwrap();
    let secrets = FileSecretSource::new();
    let dialer: Arc<dyn Dialer> = r.dialer.clone();
    let deps = RelayCheckDeps { resolver: &r.dns, dialer: dialer.as_ref(), tls: &tls, secrets: &secrets, helo: "mx.example.net" };
    check_relay(&r.cfg, &deps).await
}

fn good_relay(pki: &TestPki, mechanisms: &[&str]) -> impl Fn(usize) -> Script + Send + Sync + 'static {
    let sel = pki.server.clone();
    let mechs: Vec<String> = mechanisms.iter().map(|m| (*m).to_owned()).collect();
    move |_| Script { tls: Some(sel.clone()), require_auth: Some(("mailer".into(), "s3cret".into())), auth_mechanisms: mechs.clone(), ..Script::default() }
}

#[tokio::test]
async fn a_working_relay_is_reported_with_its_tls_and_mechanism_and_receives_no_mail() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["PLAIN"])).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    let rep = run(&r).await.unwrap();
    assert!(rep.tls.as_ref().is_some_and(|t| t.verified), "the certificate is verified against the bundle");
    assert_eq!(rep.mechanism.as_deref(), Some("PLAIN"));
    assert_eq!(rep.offered, vec!["PLAIN".to_owned()]);
    assert_eq!(rep.host, HOST);
    assert!(relay.messages().is_empty(), "a check never sends a message");
    assert_eq!(relay.connection_count(), 1);
}

#[tokio::test]
async fn a_relay_that_offers_only_login_is_usable() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["LOGIN"])).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    let rep = run(&r).await.unwrap();
    assert_eq!(rep.mechanism.as_deref(), Some("LOGIN"));
}

#[tokio::test]
async fn plain_is_preferred_when_both_are_offered() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["LOGIN", "PLAIN"])).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    assert_eq!(run(&r).await.unwrap().mechanism.as_deref(), Some("PLAIN"));
}

#[tokio::test]
async fn wrong_credentials_are_reported_as_such_and_never_echoed() {
    let pki = TestPki::new(HOST);
    let sel = pki.server.clone();
    let relay = FakeMta::start(move |_| Script { tls: Some(sel.clone()), require_auth: Some(("mailer".into(), "another-password".into())), ..Script::default() }).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    let err = run(&r).await.unwrap_err();
    assert!(matches!(err, RelayFailure::Auth), "{err:?}");
    for text in [err.to_string(), format!("{err:?}")] {
        for secret in ["s3cret", "another-password", "mailer", HOST, "bad credentials", "192.0.2.50"] {
            assert!(!text.contains(secret), "{text:?} leaks {secret:?}");
        }
    }
}

#[tokio::test]
async fn a_certificate_from_an_unknown_authority_is_a_tls_failure() {
    let pki = TestPki::new(HOST);
    let stranger = pki.stranger.clone();
    let relay = FakeMta::start(move |_| Script { tls: Some(stranger.clone()), require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls), "the certificate must be verified");
    assert!(relay.messages().is_empty());
}

#[tokio::test]
async fn a_relay_without_starttls_never_gets_the_password() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(|_| Script { require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls));
    assert!(r.dir.path().join("relay.pw").exists());
}

#[tokio::test]
async fn a_relay_offering_no_usable_mechanism_says_what_it_does_offer() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["XOAUTH2", "CRAM-MD5"])).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    match run(&r).await.unwrap_err() {
        RelayFailure::NoMechanism { offered } => assert_eq!(offered, vec!["XOAUTH2".to_owned(), "CRAM-MD5".to_owned()]),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_unreachable_relay_is_a_connect_failure() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["PLAIN"])).await;
    let mut r = rig(RelayTls::Starttls, &relay, pki);
    r.dialer = MapDialer::new(&[]);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Connect));
}

#[tokio::test]
async fn a_name_that_does_not_resolve_is_a_dns_failure() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["PLAIN"])).await;
    let mut r = rig(RelayTls::Starttls, &relay, pki);
    r.dns = StaticDns::default();
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Dns));
    assert_eq!(relay.connection_count(), 0);
}

#[tokio::test]
async fn a_relay_that_never_answers_fails_within_the_timeouts() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(|_| Script { silent: true, ..Script::default() }).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    let started = std::time::Instant::now();
    let err = run(&r).await.unwrap_err();
    assert!(matches!(err, RelayFailure::Protocol), "{err:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn nothing_to_check_without_a_relay() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["PLAIN"])).await;
    let mut r = rig(RelayTls::Starttls, &relay, pki);
    r.cfg.relay = None;
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::NotConfigured));
}

#[tokio::test]
async fn a_password_file_that_others_can_read_is_refused_before_any_connection() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(good_relay(&pki, &["PLAIN"])).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    std::fs::set_permissions(r.dir.path().join("relay.pw"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Credentials));
    assert_eq!(relay.connection_count(), 0);
}

#[tokio::test]
async fn a_relay_without_credentials_over_a_plain_link_is_checked_up_to_the_greeting() {
    // An internal relay that takes mail from this host without a login.
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(|_| Script::default()).await;
    let mut r = rig(RelayTls::None, &relay, pki);
    if let Some(rc) = r.cfg.relay.as_mut() {
        rc.username = None;
        rc.password_file = None;
    }
    let rep = run(&r).await.unwrap();
    assert!(rep.tls.is_none());
    assert_eq!(rep.mechanism, None);
}

// ---------------------------------------------------------------------------------- TLS and SSL
// The three ways a relay is reached: STARTTLS on the submission port (587, above), TLS from the first
// byte (465, "SSL" in many mail clients) and no TLS at all (25, internal relays). Whatever the mode,
// a configured login never leaves the host without a verified TLS session.

fn implicit_relay(pki: &TestPki) -> impl Fn(usize) -> Script + Send + Sync + 'static {
    let sel = pki.server.clone();
    move |_| Script { tls: Some(sel.clone()), implicit_tls: true, require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }
}

#[tokio::test]
async fn a_relay_with_tls_from_the_first_byte_is_checked_and_logged_in() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(implicit_relay(&pki)).await;
    let mut r = rig(RelayTls::Implicit, &relay, pki);
    if let Some(rc) = r.cfg.relay.as_mut() {
        rc.port = 465;
    }
    let rep = run(&r).await.unwrap();
    assert!(rep.tls.as_ref().is_some_and(|t| t.verified));
    assert_eq!(rep.mechanism.as_deref(), Some("PLAIN"));
    assert_eq!(rep.port, 465);
    assert!(relay.messages().is_empty());
}

#[tokio::test]
async fn implicit_tls_with_a_certificate_from_an_unknown_authority_is_refused() {
    let pki = TestPki::new(HOST);
    let stranger = pki.stranger.clone();
    let relay = FakeMta::start(move |_| Script { tls: Some(stranger.clone()), implicit_tls: true, require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
    let r = rig(RelayTls::Implicit, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls));
}

#[tokio::test]
async fn implicit_tls_against_a_server_that_speaks_plain_text_is_a_tls_failure_and_sends_no_password() {
    // The classic mistake: "implicit" configured for a port that is really STARTTLS (587) or plain (25).
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(|_| Script { require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
    let r = rig(RelayTls::Implicit, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls));
    assert!(relay.messages().is_empty());
}

#[tokio::test]
async fn starttls_against_an_implicit_tls_port_does_not_hang_and_is_not_a_success() {
    // The opposite mistake: STARTTLS configured for a port that wants TLS at once (465).
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(implicit_relay(&pki)).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    let started = std::time::Instant::now();
    assert!(run(&r).await.is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn a_certificate_for_another_host_name_is_refused_even_when_its_authority_is_trusted() {
    let pki = TestPki::new("other.example.net");
    let relay = FakeMta::start(good_relay(&pki, &["PLAIN"])).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls), "the name in the certificate must match the configured host");
    let pki = TestPki::new("other.example.net");
    let relay = FakeMta::start(implicit_relay(&pki)).await;
    let r = rig(RelayTls::Implicit, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls));
}

#[tokio::test]
async fn a_starttls_handshake_that_breaks_is_a_tls_failure() {
    let pki = TestPki::new(HOST);
    let sel = pki.server.clone();
    let relay = FakeMta::start(move |_| Script { tls: Some(sel.clone()), break_tls: true, require_auth: Some(("mailer".into(), "s3cret".into())), ..Script::default() }).await;
    let r = rig(RelayTls::Starttls, &relay, pki);
    assert!(matches!(run(&r).await.unwrap_err(), RelayFailure::Tls));
    assert!(relay.messages().is_empty());
}

#[tokio::test]
async fn a_relay_that_is_checked_without_tls_reports_no_tls() {
    let pki = TestPki::new(HOST);
    let relay = FakeMta::start(|_| Script::default()).await;
    let mut r = rig(RelayTls::None, &relay, pki);
    if let Some(rc) = r.cfg.relay.as_mut() {
        rc.username = None;
        rc.password_file = None;
        rc.port = 25;
    }
    let rep = run(&r).await.unwrap();
    assert!(rep.tls.is_none());
    assert_eq!(rep.port, 25);
}
