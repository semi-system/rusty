//! `[valkey]`: the shared key-value store (rate limits, sessions, read cache). Absent means off: every
//! consumer then keeps its own in-process state. The password is never inline (a file, mode 0600, read
//! through `SecretSource`), and a plain-text link is for loopback and development only.

use rusty_core::config::{Profile, StartupConfig};
use rusty_core::Error;

/// The smallest valid base (the same as the other configuration tests); the test text goes after it.
const BASE: &str = "blob_dir = \"/a/blobs\"
store = { url = \"postgres://rusty_app@db.internal:5432/rusty\", password_file = \"/run/secrets/pg\" }
";

fn parse(s: &str) -> Result<StartupConfig, Error> {
    StartupConfig::from_toml_str(&format!("{BASE}{s}"))
}

/// A configuration that passes the start-up checks apart from what the test is about: a host name and a
/// certificate for the listeners that authenticate. The `[valkey]` text goes last (a table).
fn startable(valkey: &str) -> StartupConfig {
    let head = "hostname = \"mail.example\"\n[tls]\ncert_file = \"/etc/rusty/c.pem\"\nkey_file = \"/etc/rusty/k.pem\"\n";
    let base = BASE.trim_end();
    // `blob_dir` and `store` are top-level keys: they go before the first table.
    let (top, store) = base.split_once('\n').unwrap();
    parse_raw(&format!("{top}\n{store}\nhostname = \"mail.example\"\n{}{valkey}", head.split_once('\n').unwrap().1))
}

fn parse_raw(s: &str) -> StartupConfig {
    StartupConfig::from_toml_str(s).unwrap()
}

fn invalid(s: &str) -> String {
    match parse(s) {
        Err(Error::Invalid(m)) => m,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn the_section_is_absent_by_default() {
    assert!(parse("").unwrap().valkey.is_none());
}

#[test]
fn a_minimal_section_gets_conservative_defaults() {
    let c = parse("[valkey]\nhost = \"valkey.internal\"\ntls = true\n").unwrap();
    let v = c.valkey.unwrap();
    assert_eq!(v.host, "valkey.internal");
    assert_eq!(v.port, 6379);
    assert!(v.tls);
    assert_eq!(v.connect_timeout_ms, 1000);
    assert_eq!(v.command_timeout_ms, 200, "a limiter call must not hold a login for long");
    assert_eq!(v.key_prefix, "rusty:");
    assert!(v.username.is_none() && v.password_file.is_none() && v.ca_bundle.is_none());
}

#[test]
fn credentials_the_password_is_a_file_and_a_user_name_needs_one() {
    let ok = "[valkey]\nhost = \"v\"\ntls = true\nusername = \"rusty_imap\"\npassword_file = \"/run/secrets/kv\"\n";
    assert!(parse(ok).is_ok());
    assert!(invalid("[valkey]\nhost = \"v\"\ntls = true\nusername = \"u\"\n").contains("together"));
    // A password alone is the `default` user with a password (the simple setup); a user name alone is refused.
    assert!(parse("[valkey]\nhost = \"v\"\ntls = true\npassword_file = \"/run/secrets/kv\"\n").is_ok());
    assert!(invalid("[valkey]\nhost = \"v\"\ntls = true\npassword_file = \"kv\"\n").contains("absolute"));
    assert!(invalid("[valkey]\nhost = \"v\"\ntls = true\nusername = \"u\"\npassword_file = \"kv\"\n").contains("absolute"));
    // An inline password is not a key of the schema at all.
    assert!(parse("[valkey]\nhost = \"v\"\ntls = true\npassword = \"hunter2\"\n").is_err());
}

#[test]
fn host_port_timeouts_prefix_and_paths_are_checked() {
    for bad in [
        "host = \"\"",
        "host = \"valkey://v\"",
        "host = \"a b\"",
        "host = \"v\"\nport = 0",
        "host = \"v\"\nconnect_timeout_ms = 0",
        "host = \"v\"\nconnect_timeout_ms = 10001",
        "host = \"v\"\ncommand_timeout_ms = 0",
        "host = \"v\"\ncommand_timeout_ms = 5001",
        "host = \"v\"\nkey_prefix = \"\"",
        "host = \"v\"\nkey_prefix = \"has space:\"",
        "host = \"v\"\nkey_prefix = \"ctl\\u0001:\"",
        "host = \"v\"\nca_bundle = \"relative.pem\"",
        "host = \"v\"\nunknown_key = 1",
    ] {
        assert!(parse(&format!("[valkey]\ntls = true\n{bad}\n")).is_err(), "{bad}");
    }
    let long = "x".repeat(33);
    assert!(parse(&format!("[valkey]\nhost = \"v\"\ntls = true\nkey_prefix = \"{long}:\"\n")).is_err());
    assert!(parse("[valkey]\nhost = \"v\"\ntls = true\nkey_prefix = \"rusty-prod:\"\nca_bundle = \"/etc/rusty/ca.pem\"\n").is_ok());
}

#[test]
fn plain_text_is_for_loopback_and_development_only() {
    // Loopback without TLS: fine everywhere (a sidecar or a test).
    let local = startable("[valkey]\nhost = \"127.0.0.1\"\n");
    assert!(local.check_startup(Profile::Dev).is_ok());
    // Another host without TLS: refused in production (credentials and rate-limit keys would cross the wire in clear).
    let remote = startable("[valkey]\nhost = \"valkey.internal\"\n");
    let e = remote.check_startup(Profile::Production).unwrap_err();
    assert!(matches!(&e, Error::Invalid(m) if m.contains("valkey") && m.contains("tls")), "{e:?}");
    // The development profile allows it and says so at every start.
    assert!(remote.check_startup(Profile::Dev).is_ok());
    assert!(
        remote.startup_warnings(Profile::Dev).iter().any(|w| w.contains("valkey")),
        "a plain-text link to another host must be warned about"
    );
    // With TLS the same host is fine.
    let tls = startable("[valkey]\nhost = \"valkey.internal\"\ntls = true\n");
    assert!(tls.check_startup(Profile::Production).is_ok());
}
