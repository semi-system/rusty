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

#[test]
fn the_dns_facts_are_optional_and_checked() {
    let c = admin("[admin]\npublic_origin = \"https://admin.example.org\"\n").unwrap().admin.unwrap();
    assert!(c.public_ips.is_empty() && c.spf_include.is_none() && !c.via_relay);
    let c = admin("[admin]\npublic_origin = \"https://admin.example.org\"\npublic_ips = [\"203.0.114.5\", \"2606:4700::1\"]\nspf_include = \"spf.mail.net\"\nvia_relay = true\n").unwrap().admin.unwrap();
    assert_eq!((c.public_ips.len(), c.spf_include.as_deref(), c.via_relay), (2, Some("spf.mail.net"), true));
    for bad in [
        "public_ips = [\"127.0.0.1\"]",
        "public_ips = [\"0.0.0.0\"]",
        "public_ips = [\"169.254.1.1\"]",
        "public_ips = [\"203.0.114.5\", \"203.0.114.5\"]",
        "public_ips = [\"1.1.1.1\", \"1.1.1.2\", \"1.1.1.3\", \"1.1.1.4\", \"1.1.1.5\", \"1.1.1.6\", \"1.1.1.7\", \"1.1.1.8\", \"1.1.1.9\"]",
        "public_ips = [\"not an ip\"]",
        "spf_include = \"bad name\"",
        "spf_include = \"a;b\"",
    ] {
        assert!(admin(&format!("[admin]\npublic_origin = \"https://admin.example.org\"\n{bad}\n")).is_err(), "{bad}");
    }
}
