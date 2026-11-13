//! `[store]` section: Postgres URL and TLS policy (U11).

use rusty_core::config::{Profile, SslMode, StartupConfig};
use rusty_core::Error;

const BASE: &str = "blob_dir = \"/a/blobs\"\nhostname = \"mail.example.test\"\n[tls]\ncert_file = \"/c\"\nkey_file = \"/k\"\n";

fn pg(extra: &str) -> String {
    format!(
        "{BASE}[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n{extra}"
    )
}

fn parse(s: &str) -> Result<StartupConfig, Error> {
    StartupConfig::from_toml_str(s)
}

fn invalid<T: std::fmt::Debug>(r: Result<T, Error>) {
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
}

#[test]
fn the_store_url_is_required_and_the_old_sqlite_keys_are_gone() {
    invalid(parse("blob_dir = \"/a/blobs\"\n"));
    invalid(parse(&format!("{BASE}[store]\n")));
    // Postgres is the only backend: neither `backend` nor `database_path` exists any more.
    invalid(parse(&pg("backend = \"postgres\"\n")));
    invalid(parse("database_path = \"/a/db.sqlite\"\nblob_dir = \"/a/blobs\"\n"));
}

#[test]
fn the_store_defaults_to_verify_full() {
    let c = parse(&pg("")).unwrap();
    assert_eq!(c.store.sslmode, SslMode::VerifyFull);
    assert!(c.check_startup(Profile::Production).is_ok());
}

#[test]
fn postgres_url_must_not_carry_a_password_or_parameters() {
    for url in [
        "postgres://rusty_app:hunter2@db.internal/rusty",
        "postgres://rusty_app@db.internal/rusty?sslmode=disable",
        "mysql://rusty_app@db.internal/rusty",
        "postgres://db.internal/rusty",
    ] {
        let text = format!("{BASE}[store]\nurl = \"{url}\"\n");
        match parse(&text) {
            Err(Error::Invalid(m)) => assert!(!m.contains("hunter2") && !m.contains(url), "{m}"),
            other => panic!("{url}: {other:?}"),
        }
    }
    invalid(parse(&format!("{BASE}[store]\n")));
}

#[test]
fn unknown_store_keys_and_bad_values_are_rejected() {
    invalid(parse(&pg("password = \"x\"\n")));
    invalid(parse(&pg("sslmode = \"prefer\"\n")));
    invalid(parse(&pg("pool_size = 0\n")));
    invalid(parse(&pg("pool_size = 201\n")));
    invalid(parse(&pg("lock_timeout_ms = 0\n")));
    invalid(parse(&pg("statement_timeout_ms = 3600001\n")));
    invalid(parse(&pg("password_file = \"rel\"\n")));
    invalid(parse(&pg("ca_bundle = \"rel.pem\"\n")));
    invalid(parse(&pg("sslmode = \"require\"\nca_bundle = \"/ca.pem\"\n")));
    invalid(parse(&pg("migrator_password_file = \"/m\"\n")));
    invalid(parse(&pg("migrator_user = \"a b\"\n")));
}

#[test]
fn production_refuses_unverified_tls_to_a_remote_host() {
    for mode in ["disable", "require", "verify-ca"] {
        let c = parse(&pg(&format!("sslmode = \"{mode}\"\n"))).unwrap();
        assert!(matches!(c.check_startup(Profile::Production), Err(Error::Invalid(_))), "{mode}");
        assert!(matches!(c.check_startup(Profile::Dev), Err(Error::Invalid(_))), "{mode} dev");
    }
}

#[test]
fn explicit_insecure_override_starts_with_a_warning() {
    let c = parse(&pg("sslmode = \"disable\"\ninsecure_allow_unverified_tls = true\n")).unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
    let warn = c.store.check_startup(Profile::Production, 1).unwrap_or(None);
    assert!(warn.is_some_and(|w| w.contains("NOT verified")));
}

#[test]
fn loopback_may_skip_verification_but_disable_is_dev_only() {
    let local = |mode: &str| {
        format!(
            "{BASE}[store]\nurl = \"postgres://rusty_app@127.0.0.1/rusty\"\npassword_file = \"/p\"\nsslmode = \"{mode}\"\n"
        )
    };
    let c = parse(&local("require")).unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
    let d = parse(&local("disable")).unwrap();
    assert!(matches!(d.check_startup(Profile::Production), Err(Error::Invalid(_))));
    assert!(d.check_startup(Profile::Dev).is_ok());
}

#[test]
fn production_postgres_needs_a_password_file() {
    let text = format!("{BASE}[store]\nurl = \"postgres://rusty_app@db.internal/rusty\"\n");
    let c = parse(&text).unwrap();
    assert!(matches!(c.check_startup(Profile::Production), Err(Error::Invalid(_))));
}

#[test]
fn pool_must_cover_the_enabled_services() {
    // Five listeners, GC and the outbound worker: 7 services + 4 headroom.
    let c = parse(&pg("pool_size = 10\n")).unwrap();
    assert!(matches!(c.check_startup(Profile::Production), Err(Error::Invalid(_))));
    let c = parse(&pg("pool_size = 11\n")).unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
    // Fewer listeners lower the floor.
    let c = parse(&pg("pool_size = 7\n[smtp]\nlisten_587 = \"\"\nlisten_465 = \"\"\n[imap]\nlisten_143 = \"\"\nlisten_993 = \"\"\n")).unwrap();
    assert!(c.check_startup(Profile::Production).is_ok());
}

#[test]
fn debug_output_has_no_secret() {
    let c = parse(&pg("")).unwrap();
    let text = format!("{c:?}");
    assert!(!text.contains("hunter2"));
}

#[test]
fn the_dev_profile_takes_its_postgres_server_from_a_url() {
    let dev = StartupConfig::dev(std::path::Path::new("/tmp/d"));
    // Loopback is reached without TLS; the password comes from the environment, never from the URL.
    let c = dev.clone().with_dev_postgres("postgres://rusty_dev@127.0.0.1:5433/devdb", None, None).unwrap();
    assert_eq!(c.store.url.as_deref(), Some("postgres://rusty_dev@127.0.0.1:5433/devdb"));
    assert_eq!(c.store.sslmode, SslMode::Disable);
    assert!(c.store.password_from_dev_env);
    assert!(c.check_startup(Profile::Dev).is_ok());
    // Another host keeps the verified default.
    let r = dev.clone().with_dev_postgres("postgres://rusty_dev@db.internal/devdb", None, None).unwrap();
    assert_eq!(r.store.sslmode, SslMode::VerifyFull);
    // A password in the URL, query parameters and other schemes are refused.
    for bad in ["postgres://u:pw@127.0.0.1/d", "postgres://u@127.0.0.1/d?sslmode=disable", "mysql://u@h/d", ""] {
        invalid(dev.clone().with_dev_postgres(bad, None, None));
    }
    // Naming a mode that does not verify a remote server is an explicit choice, and says so.
    let n = dev.clone().with_dev_postgres("postgres://rusty_dev@db.internal/devdb", Some(SslMode::Disable), None).unwrap();
    assert!(n.store.insecure_allow_unverified_tls && n.check_startup(Profile::Dev).is_ok());
    let v = dev.clone().with_dev_postgres("postgres://rusty_dev@db.internal/devdb", Some(SslMode::VerifyFull), Some(std::path::PathBuf::from("/ca.pem"))).unwrap();
    assert!(!v.store.insecure_allow_unverified_tls && v.store.ca_bundle.is_some());
    // A relative CA path is refused.
    invalid(dev.clone().with_dev_postgres("postgres://rusty_dev@db.internal/devdb", None, Some(std::path::PathBuf::from("ca.pem"))));
    // A configuration file cannot switch the environment password on.
    invalid(parse(&pg("password_from_dev_env = true\n")));
}
