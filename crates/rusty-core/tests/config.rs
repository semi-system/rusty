use rusty_core::config::{LogLevel, StartupConfig};
use rusty_core::Error;
use std::path::Path;

const BASE: &str = "database_path = \"/a/db.sqlite\"\nblob_dir = \"/a/blobs\"\n";

fn with(extra: &str) -> rusty_core::Result<StartupConfig> {
    StartupConfig::from_toml_str(&format!("{BASE}{extra}\n"))
}

fn invalid<T: std::fmt::Debug>(r: rusty_core::Result<T>) {
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
}

#[test]
fn defaults() {
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    assert_eq!(c.database_path, Path::new("/a/db.sqlite"));
    assert_eq!(c.blob_dir, Path::new("/a/blobs"));
    assert_eq!(c.log_level, LogLevel::Info);
    assert_eq!(c.max_message_bytes, 52_428_800);
    assert_eq!(c.gc_grace_seconds, 604_800);
}

#[test]
fn log_levels_parse() {
    assert_eq!(with("log_level = \"trace\"").unwrap().log_level, LogLevel::Trace);
    assert_eq!(with("log_level = \"error\"").unwrap().log_level, LogLevel::Error);
    invalid(with("log_level = \"loud\""));
}

#[test]
fn unknown_key_rejected() {
    invalid(with("bogus = 1"));
}

#[test]
fn missing_required_rejected() {
    invalid(StartupConfig::from_toml_str("blob_dir = \"/b\""));
}

#[test]
fn relative_paths_rejected() {
    invalid(StartupConfig::from_toml_str("database_path = \"db.sqlite\"\nblob_dir = \"/b\""));
    invalid(StartupConfig::from_toml_str("database_path = \"/d\"\nblob_dir = \"blobs\""));
}

#[test]
fn max_message_bytes_bounds() {
    assert!(with("max_message_bytes = 1024").is_ok());
    assert!(with("max_message_bytes = 1073741824").is_ok());
    invalid(with("max_message_bytes = 1023"));
    invalid(with("max_message_bytes = 1073741825"));
    invalid(with("max_message_bytes = -1"));
}

#[test]
fn gc_grace_bounds() {
    // The floor of 3600 only stands alone when no outbound retry window needs the blobs; the
    // outbound rule (below) lifts it to max_age_secs + 1 day.
    assert!(with("gc_grace_seconds = 518400").is_ok());
    invalid(with("gc_grace_seconds = 3599"));
}

#[test]
fn invalid_toml_is_invalid_and_does_not_echo() {
    let r = StartupConfig::from_toml_str("database_path = \"SECRET-VALUE\nzzz");
    match r {
        Err(Error::Invalid(m)) => assert!(!m.contains("SECRET-VALUE"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn load_from_file_and_limits() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("c.toml");
    std::fs::write(&p, BASE).unwrap();
    assert!(StartupConfig::load(&p).is_ok());

    let big = d.path().join("big.toml");
    std::fs::write(&big, format!("{BASE}#{}\n", "x".repeat(70 * 1024))).unwrap();
    invalid(StartupConfig::load(&big));

    let exact = d.path().join("exact.toml");
    let mut s = String::from(BASE);
    s.push(char::from(35u8));
    s.push_str(&"x".repeat(64 * 1024 - s.len()));
    assert_eq!(s.len(), 64 * 1024);
    std::fs::write(&exact, s).unwrap();
    assert!(StartupConfig::load(&exact).is_ok());

    assert!(matches!(
        StartupConfig::load(&d.path().join("nope.toml")),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn example_file_parses() {
    StartupConfig::from_toml_str(include_str!("../../../rusty.example.toml")).unwrap();
}

#[test]
fn log_level_is_case_sensitive() {
    invalid(with("log_level = \"INFO\""));
    invalid(with("log_level = \"Info\""));
    assert!(with("log_level = \"info\"").is_ok());
}

#[test]
fn file_of_64_kib_plus_one_is_rejected_by_the_fstat_size_check() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("over.toml");
    // 65537 bytes of valid-TOML padding: rejected by the fstat size check before reading.
    // (The post-read "too large" branch is unit-tested via read_bounded in config.rs.)
    let mut s = String::from(BASE);
    s.push(char::from(35u8));
    s.push_str(&"x".repeat(64 * 1024 + 1 - s.len()));
    assert_eq!(s.len(), 64 * 1024 + 1);
    std::fs::write(&p, s).unwrap();
    match StartupConfig::load(&p) {
        Err(Error::Invalid(m)) => assert!(m.contains("small regular file"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn load_does_not_block_on_fifo() {
    use std::sync::mpsc;
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("fifo.toml");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &p,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let p2 = p.clone();
    std::thread::spawn(move || {
        let _ = tx.send(StartupConfig::load(&p2));
    });
    let r = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("load blocked on a FIFO");
    invalid(r);
}

// ---- [outbound] (U5) ----

#[test]
fn outbound_defaults_are_safe() {
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    let o = &c.outbound;
    assert_eq!(o.max_age_secs, 5 * 86_400);
    assert_eq!(o.backoff_secs, vec![300, 900, 1800, 3600, 7200, 14_400]);
    assert_eq!(o.default_tls, rusty_core::config::TlsPolicy::Opportunistic);
    assert!(o.relay.is_none() && o.nameserver.is_none() && o.ca_bundle.is_none() && o.bind_address.is_none());
}

#[test]
fn outbound_unknown_keys_and_bad_values_are_rejected() {
    invalid(with("[outbound]\nfrobnicate = 1"));
    invalid(with("[outbound]\nmax_age_secs = 10"));
    invalid(with("[outbound]\nbackoff_secs = []"));
    invalid(with("[outbound]\nbackoff_secs = [0]"));
    invalid(with("[outbound]\nmax_concurrent_per_domain = 0"));
    invalid(with("[outbound]\nlease_secs = 5"));
    invalid(with("[outbound]\nnameserver = \"not an address\""));
    invalid(with("[outbound]\nca_bundle = \"relative.pem\""));
    invalid(with("[outbound]\ndkim_selector = \"bad selector\""));
    invalid(with("[outbound]\ndefault_tls = \"maybe\""));
    invalid(with("[outbound.tls_policy]\n\"not a domain!\" = \"required\""));
    assert!(with("[outbound]\nnameserver = \"10.77.0.10:53\"\nca_bundle = \"/etc/rusty/ca.pem\"\nbind_address = \"192.0.2.7\"").is_ok());
    let c = with("[outbound.tls_policy]\n\"Example.ORG\" = \"verified\"").unwrap();
    assert_eq!(c.outbound.tls_policy.get("example.org"), Some(&rusty_core::config::TlsPolicy::Verified));
}

#[test]
fn outbound_relay_needs_a_password_file_not_an_inline_password() {
    invalid(with("[outbound.relay]\nhost = \"smtp.example.net\"\nport = 587\npassword = \"inline\""));
    invalid(with("[outbound.relay]\nhost = \"smtp.example.net\"\nport = 587\nusername = \"u\""));
    invalid(with("[outbound.relay]\nhost = \"smtp.example.net\"\nport = 587\nusername = \"u\"\npassword_file = \"rel\""));
    let c = with("[outbound.relay]\nhost = \"smtp.example.net\"\nport = 587\ntls = \"starttls\"\nusername = \"u\"\npassword_file = \"/etc/rusty/relay.pw\"").unwrap();
    assert_eq!(c.outbound.relay.unwrap().port, 587);
}

/// U5 spike 1: a queued body has no database reference, so the blob GC collects it once it is
/// older than the grace window (see crates/rusty-smtp/tests/outbound_gc.rs). The configuration
/// therefore refuses a grace window shorter than the longest retry plus a day.
#[test]
fn gc_grace_must_outlive_the_outbound_retry_window() {
    // The default grace and the default max age are consistent.
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    assert!(c.gc_grace_seconds >= c.outbound.max_age_secs + 86_400);
    invalid(with("gc_grace_seconds = 86400"));
    invalid(with("gc_grace_seconds = 518399")); // max age 5 d + 1 d = 518400
    assert!(with("gc_grace_seconds = 518400").is_ok());
    invalid(with("gc_grace_seconds = 604800\n[outbound]\nmax_age_secs = 1209600"));
    assert!(with("gc_grace_seconds = 1296000\n[outbound]\nmax_age_secs = 1209600").is_ok());
}
