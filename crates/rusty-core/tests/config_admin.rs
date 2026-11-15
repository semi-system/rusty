//! `[admin]`: the admin web service's own section.

use rusty_core::config::Role;
use rusty_core::config::StartupConfig;

const BASE: &str = "hostname = \"admin.example.org\"\n[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n";

fn admin(extra: &str) -> rusty_core::Result<StartupConfig> {
    StartupConfig::from_toml_str_for(Role::Admin, &format!("{BASE}{extra}"))
}

#[test]
fn the_section_has_a_loopback_default_listen_address_and_needs_an_origin() {
    let c = admin("[admin]\npublic_origin = \"https://admin.example.org\"\n").unwrap();
    let a = c.admin.unwrap();
    assert_eq!(a.listen.to_string(), "127.0.0.1:8080");
    assert_eq!(a.public_origin, "https://admin.example.org");
    assert!(admin("[admin]\nlisten = \"127.0.0.1:9000\"\n").is_err(), "the origin is required");
    assert_eq!(admin("[admin]\nlisten = \"0.0.0.0:9000\"\npublic_origin = \"https://a.example.org:8443\"\n").unwrap().admin.unwrap().listen.port(), 9000);
}

#[test]
fn a_bad_origin_or_an_unknown_key_is_refused() {
    for bad in ["http://a.example.org", "https://A.example.org", "https://a.example.org/", "a.example.org", ""] {
        assert!(admin(&format!("[admin]\npublic_origin = \"{bad}\"\n")).is_err(), "{bad:?}");
    }
    assert!(admin("[admin]\npublic_origin = \"https://a.example.org\"\nextra = 1\n").is_err());
}

#[test]
fn only_the_admin_role_accepts_the_section() {
    for role in [Role::SmtpIn, Role::Submission, Role::Imap, Role::Outbound, Role::Gc] {
        let text = format!("{BASE}blob_dir = \"/var/lib/rusty/blobs\"\n[admin]\npublic_origin = \"https://a.example.org\"\n");
        let e = StartupConfig::from_toml_str_for(role, &text).unwrap_err().to_string();
        assert!(e.contains("[admin] is not accepted"), "{role:?}: {e}");
        assert!(!e.contains("a.example.org"), "the value is never echoed");
    }
}
