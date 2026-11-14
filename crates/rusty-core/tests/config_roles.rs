//! `StartupConfig::from_toml_str_for(role, text)`: one configuration file per service, and a service is refused a
//! file that carries sections it does not use (image design section 4), so it can never be handed a secret that
//! it has no business with. The expected matrix below is written out by hand, independently of the code, so a
//! section that is widened by accident shows up as a failing row.

use rusty_core::config::{Role, StartupConfig};
use rusty_core::Error;

/// Every section and key of the schema that a configuration may carry, with a valid example.
const SECTIONS: &[(&str, &str)] = &[
    ("blob_dir", "blob_dir = \"/a/blobs\"\n"),
    ("log_level", "log_level = \"info\"\n"),
    ("log_format", "log_format = \"json\"\n"),
    ("hostname", "hostname = \"mail.example.test\"\n"),
    ("max_message_bytes", "max_message_bytes = 10485760\n"),
    ("gc_grace_seconds", "gc_grace_seconds = 2000000\n"),
    ("store", "[store]\nurl = \"postgres://rusty_app@db.internal:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n"),
    ("tls", "[tls]\ncert_file = \"/etc/rusty/c.pem\"\nkey_file = \"/etc/rusty/k.pem\"\n"),
    ("smtp", "[smtp]\nmax_recipients = 50\n"),
    ("imap", "[imap]\nmax_connections = 50\n"),
    ("outbound", "[outbound]\nmax_concurrent = 10\n"),
    ("limits", "[limits]\nshutdown_grace_secs = 20\n"),
    ("dns_check", "[dns_check]\nenabled = true\n"),
    ("valkey", "[valkey]\nhost = \"kv.internal\"\nport = 6379\ntls = true\nusername = \"rusty\"\npassword_file = \"/run/secrets/kv\"\n"),
];

fn allowed(role: Role) -> &'static [&'static str] {
    // Written out per role (not computed): the point of the matrix is to be an independent statement.
    match role {
        Role::SmtpIn => &["hostname", "log_level", "log_format", "store", "limits", "blob_dir", "max_message_bytes", "tls", "smtp"],
        Role::Submission => &["hostname", "log_level", "log_format", "store", "limits", "blob_dir", "max_message_bytes", "tls", "smtp", "outbound", "valkey"],
        Role::Imap => &["hostname", "log_level", "log_format", "store", "limits", "blob_dir", "max_message_bytes", "tls", "imap", "valkey"],
        Role::Outbound => &["hostname", "log_level", "log_format", "store", "limits", "blob_dir", "outbound"],
        Role::Gc => &["hostname", "log_level", "log_format", "store", "limits", "blob_dir", "gc_grace_seconds", "dns_check"],
        Role::Admin => &["hostname", "log_level", "log_format", "store", "limits", "tls", "valkey"],
    }
}

const ROLES: [Role; 6] = [Role::SmtpIn, Role::Submission, Role::Imap, Role::Outbound, Role::Gc, Role::Admin];

/// A file with exactly the keys in `names` (top-level keys first, then the tables).
fn file(names: &[&str]) -> String {
    let mut top = String::new();
    let mut tables = String::new();
    for (name, text) in SECTIONS {
        if names.contains(name) {
            if text.starts_with('[') {
                tables.push_str(text);
            } else {
                top.push_str(text);
            }
        }
    }
    format!("{top}{tables}")
}

fn needs_blob_dir(role: Role) -> bool {
    role != Role::Admin
}

#[test]
fn every_role_accepts_exactly_its_own_sections() {
    for role in ROLES {
        let mut names: Vec<&str> = allowed(role).to_vec();
        // The all-sections file of a role is accepted.
        let ok = StartupConfig::from_toml_str_for(role, &file(&names));
        assert!(ok.is_ok(), "{role:?} refused its own sections: {:?}", ok.err());
        // Each section the role does NOT use is refused, one at a time, and nothing else changes.
        names.sort_unstable();
        for (name, _) in SECTIONS {
            if allowed(role).contains(name) {
                continue;
            }
            let mut with = allowed(role).to_vec();
            with.push(name);
            let r = StartupConfig::from_toml_str_for(role, &file(&with));
            assert!(matches!(r, Err(Error::Invalid(_))), "{role:?} accepted [{name}]");
        }
    }
}

#[test]
fn the_refusal_names_the_section_and_the_service_and_nothing_else() {
    let text = format!("{}[outbound.relay]\nhost = \"smtp.secret-relay.example\"\nport = 587\nusername = \"u\"\npassword_file = \"/run/secrets/relay-password\"\n", file(allowed(Role::Imap)));
    let err = StartupConfig::from_toml_str_for(Role::Imap, &text).unwrap_err().to_string();
    assert!(err.contains("outbound") && err.contains("imap"), "{err}");
    for leak in ["secret-relay", "relay-password", "587", "smtp.", "/run/secrets"] {
        assert!(!err.contains(leak), "{err}");
    }
}

#[test]
fn a_key_that_is_not_in_the_schema_is_refused_for_every_role_without_echoing_it() {
    for role in ROLES {
        // Before every table, so it is a top-level key of an otherwise complete file.
        let text = format!("surprise = \"x\"\n{}", file(allowed(role)));
        let r = StartupConfig::from_toml_str_for(role, &text);
        assert!(matches!(r, Err(Error::Invalid(_))), "{role:?}");
        assert!(!r.unwrap_err().to_string().contains("surprise"));
    }
}

#[test]
fn only_the_roles_that_touch_blobs_need_a_blob_dir_and_the_admin_may_not_have_one() {
    for role in ROLES {
        let without: Vec<&str> = allowed(role).iter().copied().filter(|n| *n != "blob_dir").collect();
        let r = StartupConfig::from_toml_str_for(role, &file(&without));
        if needs_blob_dir(role) {
            assert!(r.is_err(), "{role:?} must refuse a file without blob_dir");
        } else {
            assert!(r.is_ok(), "{role:?} needs no blob_dir: {:?}", r.err());
        }
    }
    let with = format!("blob_dir = \"/a/blobs\"\n{}", file(allowed(Role::Admin)));
    assert!(StartupConfig::from_toml_str_for(Role::Admin, &with).is_err(), "the admin is not handed a blob directory");
}

#[test]
fn a_role_file_is_validated_like_any_other_configuration() {
    // The same bounds as ever: a bad value in an allowed section is still refused.
    let bad = file(allowed(Role::Imap)).replace("max_message_bytes = 10485760", "max_message_bytes = 5");
    assert!(StartupConfig::from_toml_str_for(Role::Imap, &bad).is_err());
    assert!(StartupConfig::from_toml_str_for(Role::Gc, "this is not toml").is_err());
}

#[test]
fn the_file_of_one_role_is_still_a_valid_all_in_one_configuration_when_it_carries_a_blob_dir() {
    // The all-in-one loader (`from_toml_str`) is unchanged: it knows no roles.
    let text = file(allowed(Role::Imap));
    let a = StartupConfig::from_toml_str(&text).unwrap();
    let b = StartupConfig::from_toml_str_for(Role::Imap, &text).unwrap();
    assert_eq!(a, b);
}

#[test]
fn an_admin_file_gets_no_usable_blob_directory() {
    let c = StartupConfig::from_toml_str_for(Role::Admin, &file(allowed(Role::Admin))).unwrap();
    assert!(!Role::Admin.uses_blobs());
    assert!(Role::Imap.uses_blobs());
    // Whatever stands in for it is not a path a service could mistake for real storage.
    assert!(c.blob_dir.starts_with("/nonexistent"), "{:?}", c.blob_dir);
}

#[test]
fn role_names_are_stable() {
    let names: Vec<&str> = ROLES.iter().map(|r| r.name()).collect();
    assert_eq!(names, ["smtp-in", "submission", "imap", "outbound", "gc", "admin"]);
    for r in ROLES {
        assert_eq!(Role::from_name(r.name()), Some(r));
    }
    assert_eq!(Role::from_name("ctl"), None);
    assert_eq!(Role::from_name(""), None);
}
