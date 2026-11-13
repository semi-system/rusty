//! U1: the service sections of the startup configuration (hostname, [tls], [smtp], [imap],
//! [outbound], [limits]) and the start-up profile checks. Strict: unknown keys are rejected.

use rusty_core::config::{LogFormat, Profile, StartupConfig};
use rusty_core::Error;
use std::net::SocketAddr;
use std::path::Path;

const BASE: &str = "blob_dir = \"/a/blobs\"\nstore = { url = \"postgres://rusty_app@db.internal:5432/rusty\", password_file = \"/run/secrets/pg\" }\n";

fn with(extra: &str) -> rusty_core::Result<StartupConfig> {
    StartupConfig::from_toml_str(&format!("{BASE}{extra}\n"))
}

fn invalid<T: std::fmt::Debug>(r: rusty_core::Result<T>) {
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

#[test]
fn service_defaults_are_the_standard_ports_and_safe() {
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    assert_eq!(c.hostname, None);
    assert_eq!(c.log_format, LogFormat::Text);
    assert_eq!(c.tls.cert_file, None);
    assert_eq!(c.tls.key_file, None);
    assert!(!c.tls.dev_self_signed);
    assert_eq!(c.smtp.listen_25.addr(), Some(addr("0.0.0.0:25")));
    assert_eq!(c.smtp.listen_587.addr(), Some(addr("0.0.0.0:587")));
    assert_eq!(c.smtp.listen_465.addr(), Some(addr("0.0.0.0:465")));
    assert_eq!(c.imap.listen_143.addr(), Some(addr("0.0.0.0:143")));
    assert_eq!(c.imap.listen_993.addr(), Some(addr("0.0.0.0:993")));
    assert_eq!(c.smtp.max_recipients, 100);
    assert!(!c.smtp.proxy_protocol && !c.imap.proxy_protocol);
    assert_eq!(c.smtp_max_message_bytes(), c.max_message_bytes);
}

#[test]
fn sections_parse() {
    let c = with(
        "hostname = \"mail.example.com\"\nlog_format = \"json\"\n\
         [tls]\ncert_file = \"/etc/rusty/cert.pem\"\nkey_file = \"/etc/rusty/key.pem\"\n\
         [smtp]\nlisten_25 = \"127.0.0.1:2525\"\nlisten_587 = \"\"\nmax_recipients = 50\n\
         max_message_bytes = 1048576\nmax_connections = 10\nmax_connections_per_ip = 2\n\
         [imap]\nlisten_143 = \"[::1]:1143\"\nmax_connections = 7\n\
         [outbound]\n[limits]\nshutdown_grace_secs = 5\nauth_failures_max = 3\n",
    )
    .unwrap();
    assert_eq!(c.hostname.as_deref(), Some("mail.example.com"));
    assert_eq!(c.log_format, LogFormat::Json);
    assert_eq!(c.tls.cert_file.as_deref(), Some(Path::new("/etc/rusty/cert.pem")));
    assert_eq!(c.smtp.listen_25.addr(), Some(addr("127.0.0.1:2525")));
    assert_eq!(c.smtp.listen_587.addr(), None, "an empty string disables a listener");
    assert_eq!(c.smtp.max_recipients, 50);
    assert_eq!(c.smtp_max_message_bytes(), 1_048_576);
    assert_eq!(c.imap.listen_143.addr(), Some(addr("[::1]:1143")));
    assert_eq!(c.limits.shutdown_grace_secs, 5);
    assert_eq!(c.limits.auth_failures_max, 3);
}

#[test]
fn unknown_keys_are_rejected_in_every_section() {
    invalid(with("[tls]\nbogus = 1"));
    invalid(with("[smtp]\nbogus = 1"));
    invalid(with("[imap]\nbogus = 1"));
    invalid(with("[outbound]\nbogus = 1"));
    invalid(with("[limits]\nbogus = 1"));
    invalid(with("[nosuch]\nx = 1"));
}

#[test]
fn bad_values_are_rejected_without_echo() {
    invalid(with("[smtp]\nlisten_25 = \"not-an-address\""));
    invalid(with("[smtp]\nlisten_25 = \"0.0.0.0\""));
    invalid(with("[smtp]\nmax_recipients = 0"));
    invalid(with("[smtp]\nmax_recipients = 100001"));
    invalid(with("[smtp]\nmax_connections = 0"));
    invalid(with("[smtp]\nmax_message_bytes = 10"));
    // The SMTP cap cannot exceed the mailbox cap: SMTP would accept what the store refuses.
    invalid(with("max_message_bytes = 2048\n[smtp]\nmax_message_bytes = 4096"));
    invalid(with("[limits]\nshutdown_grace_secs = 0"));
    invalid(with("[limits]\nshutdown_grace_secs = 601"));
    invalid(with("hostname = \"bad host\""));
    invalid(with("hostname = \"\""));
    invalid(with("hostname = \"a..b\""));
    invalid(with("hostname = \"-a.example\""));
    match with("[smtp]\nlisten_25 = \"SECRET-VALUE\"") {
        Err(Error::Invalid(m)) => assert!(!m.contains("SECRET-VALUE"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn tls_section_rules() {
    // cert and key come as a pair, by absolute path, and not together with the dev cert.
    invalid(with("[tls]\ncert_file = \"/c.pem\""));
    invalid(with("[tls]\nkey_file = \"/k.pem\""));
    invalid(with("[tls]\ncert_file = \"c.pem\"\nkey_file = \"/k.pem\""));
    invalid(with("[tls]\ncert_file = \"/c.pem\"\nkey_file = \"k.pem\""));
    invalid(with("[tls]\ncert_file = \"/c.pem\"\nkey_file = \"/k.pem\"\ndev_self_signed = true"));
    assert!(with("[tls]\ndev_self_signed = true").is_ok());
}

#[test]
fn proxy_protocol_true_is_rejected_until_the_edge_seam_is_used() {
    assert!(with("[smtp]\nproxy_protocol = false").is_ok());
    invalid(with("[smtp]\nproxy_protocol = true"));
    invalid(with("[imap]\nproxy_protocol = true"));
}

#[test]
fn production_profile_needs_hostname() {
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    invalid(c.check_startup(Profile::Production));
    let c = with("hostname = \"mail.example.com\"\n[tls]\ndev_self_signed = true").unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
}

#[test]
fn production_without_cert_and_without_dev_flag_refuses_authenticated_listeners() {
    // Defaults enable 587/465/143/993, which accept authentication: no TLS source, no start.
    let c = with("hostname = \"mail.example.com\"").unwrap();
    match c.check_startup(Profile::Production) {
        Err(Error::Invalid(m)) => assert!(m.contains("TLS"), "{m}"),
        other => panic!("{other:?}"),
    }
    // Only port 25 (never offers AUTH) may run without a certificate.
    let c = with(
        "hostname = \"mail.example.com\"\n[smtp]\nlisten_587 = \"\"\nlisten_465 = \"\"\n\
         [imap]\nlisten_143 = \"\"\nlisten_993 = \"\"",
    )
    .unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
    // A configured cert pair satisfies it.
    let c = with(
        "hostname = \"mail.example.com\"\n[tls]\ncert_file = \"/c.pem\"\nkey_file = \"/k.pem\"",
    )
    .unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
}

#[test]
fn dev_profile_uses_high_ports_on_loopback_and_the_dev_cert() {
    let c = StartupConfig::dev(Path::new("/tmp/rusty-dev"));
    assert_eq!(c.blob_dir, Path::new("/tmp/rusty-dev/blobs"));
    assert_eq!(c.smtp.listen_25.addr(), Some(addr("127.0.0.1:2525")));
    assert_eq!(c.smtp.listen_587.addr(), Some(addr("127.0.0.1:5587")));
    assert_eq!(c.smtp.listen_465.addr(), Some(addr("127.0.0.1:4465")));
    assert_eq!(c.imap.listen_143.addr(), Some(addr("127.0.0.1:1143")));
    assert_eq!(c.imap.listen_993.addr(), Some(addr("127.0.0.1:9993")));
    assert!(c.tls.dev_self_signed);
    assert!(c.check_startup(Profile::Dev).is_ok());
}

#[test]
fn example_file_documents_the_new_sections() {
    let text = include_str!("../../../rusty.example.toml");
    for key in ["hostname", "[tls]", "dev_self_signed", "[smtp]", "[imap]", "[limits]", "log_format"] {
        assert!(text.contains(key), "rusty.example.toml does not mention {key}");
    }
}

// ---------------------------------------------------------------- [dns_check] (U10 part 1b)

#[test]
fn dns_check_is_off_by_default_with_conservative_values() {
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    assert!(!c.dns_check.enabled);
    assert_eq!(c.dns_check.interval_secs, 86_400);
    assert_eq!(c.dns_check.jitter_secs, 3_600);
    assert_eq!(c.dns_check.alert_after_failures, 3);
    assert_eq!(c.dns_check.nameserver, None);
    assert_eq!(c.dns_check.second_nameserver, None);
}

#[test]
fn dns_check_accepts_valid_values() {
    let c = with(
        "[dns_check]\nenabled = true\ninterval_secs = 43200\njitter_secs = 600\nalert_after_failures = 5\n\
         nameserver = \"9.9.9.9\"\nsecond_nameserver = \"[2606:4700:4700::1111]:53\"",
    )
    .unwrap();
    assert!(c.dns_check.enabled);
    assert_eq!(c.dns_check.interval_secs, 43_200);
    assert_eq!(c.dns_check.nameserver.map(|n| n.addr()), Some(addr("9.9.9.9:53")), "the port defaults to 53");
    assert_eq!(c.dns_check.second_nameserver.map(|n| n.addr()), Some(addr("[2606:4700:4700::1111]:53")));
}

#[test]
fn dns_check_rejects_bad_values_without_echoing_them() {
    for bad in [
        "interval_secs = 59",
        "interval_secs = 2592001",
        "jitter_secs = 100000",
        "interval_secs = 3600\njitter_secs = 3601",
        "alert_after_failures = 0",
        "alert_after_failures = 101",
        "nameserver = \"not-an-address\"",
        "nameserver = \"dns.example.com\"",
        "nameserver = \"\"",
        "nameserver = \"1.1.1.1:0\"",
        "enabled = \"yes\"",
        "unknown_key = 1",
    ] {
        let r = with(&format!("[dns_check]\n{bad}"));
        match r {
            Err(Error::Invalid(m)) => assert!(!m.contains("not-an-address") && !m.contains("dns.example.com"), "{m}"),
            other => panic!("{bad}: {other:?}"),
        }
    }
}

#[test]
fn dns_check_two_nameservers_must_differ() {
    invalid(with("[dns_check]\nnameserver = \"9.9.9.9\"\nsecond_nameserver = \"9.9.9.9\""));
    invalid(with("[dns_check]\nsecond_nameserver = \"9.9.9.9\""));
}
